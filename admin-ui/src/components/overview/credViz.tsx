import { useCallback, useState, type ReactNode } from 'react'
import { createPortal } from 'react-dom'
import { useTranslation } from 'react-i18next'
import i18n from '@/i18n'
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

/** 健康态 i18n key（图例用，渲染时 t）。 */
export const HEALTH_LABEL_KEYS: Record<Health, string> = {
  healthy: 'overviewpage.health.healthy',
  warn: 'overviewpage.health.withFailure',
  disabled: 'overviewpage.health.disabled',
}

/** 相对时间：与 StatusHeatmap 一致的“x 秒/分钟/小时前”。每次调用取当前语言。 */
export function fmtAgo(ts: number): string {
  const diff = Date.now() - ts
  if (diff < 0) return i18n.t('credentialcard.lastUsed.justNow')
  const s = Math.floor(diff / 1000)
  if (s < 5) return i18n.t('credentialcard.lastUsed.justNow')
  if (s < 60) return i18n.t('credentialcard.lastUsed.secondsAgo', { n: s })
  const m = Math.floor(s / 60)
  if (m < 60) return i18n.t('credentialcard.lastUsed.minutesAgo', { n: m })
  const h = Math.floor(m / 60)
  if (h < 24) return i18n.t('credentialcard.lastUsed.hoursAgo', { n: h })
  return i18n.t('credentialcard.lastUsed.daysAgo', { n: Math.floor(h / 24) })
}

/** 状态文案：禁用给原因，启用有失败给次数，否则健康。 */
export function statusText(c: CredentialStatusItem): string {
  if (c.disabled) return disabledReasonLabel(c.disabledReason) || i18n.t('overviewpage.health.disabled')
  if (c.failureCount > 0) return i18n.t('overviewpage.status.enabledWithFailures', { n: c.failureCount })
  return i18n.t('overviewpage.health.healthy')
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
  const { t } = useTranslation()
  return (
    <div className="space-y-1">
      <div className="flex items-center gap-2 font-medium text-foreground">
        <span>#{c.id}</span>
        {c.isCurrent && <span className="text-emerald-400">{t('overviewpage.tooltip.currentActive')}</span>}
      </div>
      {c.email && <div className="text-muted-foreground">{c.email}</div>}
      <div className="text-muted-foreground">{t('overviewpage.tooltip.auth', { method: authLabel(c.authMethod) })}</div>
      <div className="flex gap-3 tabular-nums text-muted-foreground">
        <span className="text-emerald-400">{t('overviewpage.tooltip.success', { n: c.successCount })}</span>
        <span className={c.failureCount > 0 ? 'text-red-400' : ''}>{t('overviewpage.tooltip.failure', { n: c.failureCount })}</span>
      </div>
      <div className="text-muted-foreground">{t('overviewpage.tooltip.status', { status: statusText(c) })}</div>
      <div className="text-muted-foreground">
        {t('overviewpage.tooltip.lastHit', {
          time: act ? fmtAgo(act.lastTs) : t('overviewpage.tooltip.noHit24h'),
        })}
      </div>
      {typeof c.inflight === 'number' && c.inflight > 0 && (
        <div className="text-primary">{t('overviewpage.tooltip.inflight', { n: c.inflight })}</div>
      )}
      {/* TODO(BE-balance): 额度/积分待后端批量缓存端点 cached-balances；
          本视图 hover 仅展示免费字段，绝不在此拉 per-account balance（避免触发上游风控）。 */}
    </div>
  )
}

// 悬浮卡估算尺寸（clamp 防出界用）：正文约 6~7 行小字 + 内边距，取保守上限。
const HOVER_W = 240
const HOVER_H = 190

/**
 * 鼠标跟随的悬浮卡（替代 Radix Tooltip 固定 side 的边缘翻转）。
 * dwgx：卡片要黏在鼠标上，别翻到方块左边显得脱离。
 *
 * 定位思路参照 usage-page 的 RequestPopover——文档坐标（pageX/pageY）+ createPortal 挂到 body，
 * 随页面一起滚（absolute 定位）；区别是本卡在 onMouseMove 里持续更新坐标，让它跟着鼠标走。
 * clamp：默认落在鼠标右下方 14px，右/下越界则翻到左/上，保证不出屏。
 * pointer-events-none：卡本身不吃鼠标事件，避免挡住下面方块的 hover / 抖动。
 */
function HoverCard({ x, y, children }: { x: number; y: number; children: ReactNode }) {
  // x/y 为文档坐标；用视口宽高 + 当前滚动量换算越界，右/下溢出则向左/上翻。
  const maxLeft = window.scrollX + window.innerWidth - HOVER_W - 8
  const maxTop = window.scrollY + window.innerHeight - HOVER_H - 8
  const left = Math.max(window.scrollX + 8, Math.min(x + 14, maxLeft))
  const top = Math.max(window.scrollY + 8, Math.min(y + 14, maxTop))
  return createPortal(
    <div
      className="pointer-events-none absolute z-50 overflow-hidden rounded-md border border-border bg-popover px-3 py-2 text-xs text-popover-foreground shadow-lg animate-rise-in"
      style={{ left, top }}
    >
      {children}
    </div>,
    document.body,
  )
}

/** 悬浮卡状态：正在 hover 的凭据 + 鼠标文档坐标。 */
interface HoverState {
  c: CredentialStatusItem
  x: number
  y: number
}

/**
 * 三视图共用的“鼠标跟随悬浮卡”hook：统一封装 hover 状态 + 事件处理。
 * 用法：拿到 { hover, show, hide, move, render }，
 * 给每个可 hover 元素挂 onMouseEnter={() => show(c, e)} / onMouseMove={move} / onMouseLeave={hide}，
 * 再在组件末尾放 {render(act)} 输出悬浮卡（正文仍是 CredTooltipBody，展示内容不变）。
 */
export function useHoverCard() {
  const [hover, setHover] = useState<HoverState | null>(null)

  const show = useCallback((c: CredentialStatusItem, e: { pageX: number; pageY: number }) => {
    setHover({ c, x: e.pageX, y: e.pageY })
  }, [])

  const move = useCallback((e: { pageX: number; pageY: number }) => {
    // 只在已显示时更新坐标，让卡片黏着鼠标移动。
    setHover((prev) => (prev ? { ...prev, x: e.pageX, y: e.pageY } : prev))
  }, [])

  const hide = useCallback(() => setHover(null), [])

  // 传入“按 id 取 activity”的取值函数，渲染当前 hover 凭据的悬浮卡。
  const render = useCallback(
    (getAct?: (id: number) => CellActivity | undefined) =>
      hover ? (
        <HoverCard x={hover.x} y={hover.y}>
          <CredTooltipBody c={hover.c} act={getAct?.(hover.c.id)} />
        </HoverCard>
      ) : null,
    [hover],
  )

  return { hover, show, move, hide, render }
}

/** 空池优雅占位：三视图共用。 */
export function EmptyPool({ className }: { className?: string }) {
  const { t } = useTranslation()
  return (
    <div
      className={`flex min-h-[140px] flex-col items-center justify-center gap-2 rounded-lg border border-dashed border-border/60 text-center ${className ?? ''}`}
    >
      <div className="h-2 w-2 rounded-full bg-muted-foreground/40" />
      <p className="text-sm text-muted-foreground">{t('overviewpage.kpi.totalCreds.empty')}</p>
      <p className="text-xs text-muted-foreground/60">{t('overviewpage.empty.poolHint')}</p>
    </div>
  )
}
