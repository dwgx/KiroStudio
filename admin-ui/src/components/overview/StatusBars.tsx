import type { CredentialStatusItem, CachedBalanceItem } from '@/types/api'
import { Tooltip, TooltipTrigger, TooltipContent, TooltipProvider } from '@/components/ui/tooltip'
import type { CellActivity } from '@/components/overview/StatusHeatmap'
import { healthOf, HEALTH_RGB, CredTooltipBody, EmptyPool } from '@/components/overview/credViz'
import { useCachedBalances } from '@/hooks/use-credentials'
import { subscriptionLabel } from '@/lib/i18n-labels'

export interface StatusBarsProps {
  credentials: CredentialStatusItem[]
  /** credential_id -> 实时活动；来请求时对应条闪一下。 */
  activity?: Map<number, CellActivity>
  /**
   * 可选：id(字符串) -> 已缓存余额快照。由概览页 useCachedBalances 传入即可全页共享一份。
   * 不传时本组件自行订阅缓存端点（react-query 去重，不产生额外请求）。
   * 只读缓存，绝不触发上游（封号红线）。
   */
  balances?: Record<string, CachedBalanceItem>
  className?: string
}

// 金额短显：整数直出，小数保留一位（与凭据卡口径一致）。
function fmtAmount(n: number): string {
  return Number.isInteger(n) ? String(n) : n.toFixed(1)
}

// 密集条带里的超短相对时间：5s / 3m / 2h / 4d（tooltip 里仍走完整“x 分钟前”）。
function agoShort(ts: number): string {
  const s = Math.floor((Date.now() - ts) / 1000)
  if (s < 5) return '刚刚'
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h`
  return `${Math.floor(h / 24)}d`
}

// 剩余百分比 → 前景/进度条配色（与凭据卡一致：40% 以上绿、20% 以上黄、以下红）。
function pctTone(pct: number): { text: string; bar: string } {
  if (pct >= 40) return { text: 'text-emerald-400/90', bar: 'bg-emerald-500' }
  if (pct >= 20) return { text: 'text-yellow-400/90', bar: 'bg-yellow-500' }
  return { text: 'text-red-400/90', bar: 'bg-red-500' }
}

/**
 * StatusBars —— 横向状态条带（工程感、信息密度高，运维面板风格）。
 *
 * 每个号一条细横条：左侧健康色带 + 健康灯 + #id + 邮箱(无则“无邮箱”占位) + 订阅等级，
 * 右侧一排紧凑指标：余额迷你进度条(剩余%) / RPM / 在途 / 成功·失败 / 最后活跃。
 * 无 email 的号不再留空占位（旧 bug：整条空荡荡），改由这排指标把条体填满、右对齐。
 * 余额只读缓存端点（零上游、不封号），拿不到就优雅省略该格并保留等宽占位以维持列对齐。
 * 当前活跃号常驻高亮环，来请求时整条泛起一次横向扫光。纯 CSS + Radix Tooltip，motion-reduce 降级。
 */
export function StatusBars({ credentials, activity, balances, className }: StatusBarsProps) {
  // 缓存余额：优先用外部传入（全页共享），否则自订阅（react-query 去重，零额外上游）。
  const { data: cached } = useCachedBalances()
  const balanceMap = balances ?? cached?.balances

  if (credentials.length === 0) {
    return <EmptyPool className={className} />
  }

  return (
    <TooltipProvider delayDuration={80}>
      <div className={`flex flex-col gap-1.5 ${className ?? ''}`}>
        {credentials.map((c) => {
          const h = healthOf(c)
          const rgb = HEALTH_RGB[h]
          const act = activity?.get(c.id)
          const lit = h !== 'disabled'
          const inflight = c.inflight ?? 0
          const rpm = c.rpm ?? 0

          // 订阅等级：凭据持久化字段优先，回退缓存快照；缺失则不显（不占“未知”）。
          const sub = c.subscriptionTitle ?? balanceMap?.[String(c.id)]?.subscriptionTitle ?? null

          // 余额迷你条：仅当缓存有该号且 limit 有效时展示剩余%，否则等宽占位保持列对齐。
          const bal = balanceMap?.[String(c.id)]
          const limit = bal?.usageLimit ?? 0
          const remaining = bal?.remaining ?? 0
          const remainingPct =
            bal && limit > 0 ? Math.min(Math.max((remaining / limit) * 100, 0), 100) : null

          // 最后活跃：实时命中时间优先，回退凭据 lastUsedAt（ISO）。
          const lastTs = act?.lastTs ?? (c.lastUsedAt ? Date.parse(c.lastUsedAt) : NaN)
          const hasLast = Number.isFinite(lastTs)

          return (
            <Tooltip key={c.id}>
              <TooltipTrigger asChild>
                <div
                  className={`group relative flex h-9 cursor-pointer items-center gap-2.5 overflow-hidden rounded-md border pl-4 pr-3 transition-colors duration-150 hover:border-border-hover hover:bg-foreground/[0.02] ${
                    c.isCurrent ? 'ring-1 ring-primary/60' : ''
                  }`}
                  style={{
                    borderColor: `rgb(${rgb} / ${lit ? 0.35 : 0.25})`,
                    // 极淡的整条底色，像机架上的通道灯
                    background: `linear-gradient(90deg, rgb(${rgb} / ${lit ? 0.16 : 0.09}), rgb(${rgb} / 0.03) 56px, transparent 46%)`,
                  }}
                >
                  {/* 左侧健康色带 */}
                  <span
                    className="pointer-events-none absolute inset-y-0 left-0 w-[3px]"
                    style={{ background: `rgb(${rgb} / ${lit ? 0.9 : 0.55})` }}
                  />
                  {/* 健康指示灯 */}
                  <span
                    className="h-2 w-2 shrink-0 rounded-full"
                    style={{
                      background: `rgb(${rgb} / 0.95)`,
                      boxShadow: lit ? `0 0 6px rgb(${rgb} / 0.7)` : 'none',
                    }}
                  />
                  {/* 号标识 */}
                  <span className="shrink-0 font-mono text-xs tabular-nums text-foreground">
                    #{c.id}
                  </span>
                  {/* 邮箱 / 无邮箱占位（无邮箱时不再留空白，给出灰化文案填充） */}
                  <span
                    className={`min-w-0 flex-1 truncate text-xs ${
                      c.email ? 'text-muted-foreground' : 'italic text-muted-foreground/40'
                    }`}
                  >
                    {c.email || '无邮箱'}
                  </span>
                  {/* 订阅等级（有才显） */}
                  {sub && (
                    <span className="hidden shrink-0 rounded bg-secondary/70 px-1.5 py-0.5 font-mono text-[9px] uppercase tracking-wide text-muted-foreground/90 sm:inline-block">
                      {subscriptionLabel(sub)}
                    </span>
                  )}
                  {/* 余额迷你进度条：剩余% + 细条；无缓存则等宽占位维持列对齐（不显“未知”） */}
                  {remainingPct !== null ? (
                    <div
                      className="flex w-[92px] shrink-0 items-center gap-1.5"
                      title={`剩余 ${fmtAmount(remaining)} / ${fmtAmount(limit)}`}
                    >
                      <span
                        className={`w-8 shrink-0 text-right font-mono text-[10px] tabular-nums ${pctTone(remainingPct).text}`}
                      >
                        {remainingPct.toFixed(0)}%
                      </span>
                      <span className="relative h-1.5 flex-1 overflow-hidden rounded-full bg-secondary">
                        <span
                          className={`absolute inset-y-0 left-0 rounded-full transition-all duration-500 ease-out-expo ${pctTone(remainingPct).bar}`}
                          style={{ width: `${remainingPct}%` }}
                        />
                      </span>
                    </div>
                  ) : (
                    <span className="w-[92px] shrink-0" aria-hidden />
                  )}
                  {/* RPM（近 60s，有才显） */}
                  {rpm > 0 && (
                    <span className="shrink-0 rounded bg-sky-500/10 px-1.5 py-0.5 font-mono text-[10px] tabular-nums text-sky-300/90">
                      {rpm}
                      <span className="text-[8px] text-sky-300/60">/m</span>
                    </span>
                  )}
                  {/* 在途徽标 */}
                  {inflight > 0 && (
                    <span className="shrink-0 rounded bg-primary/15 px-1.5 py-0.5 font-mono text-[10px] tabular-nums text-primary">
                      在途 {inflight}
                    </span>
                  )}
                  {/* 成功 / 失败计数（等宽右对齐） */}
                  <span className="w-10 shrink-0 text-right font-mono text-xs tabular-nums text-emerald-400/90">
                    {c.successCount}
                  </span>
                  <span
                    className={`w-8 shrink-0 text-right font-mono text-xs tabular-nums ${
                      c.failureCount > 0 ? 'text-red-400/80' : 'text-muted-foreground/25'
                    }`}
                  >
                    {c.failureCount > 0 ? `✕${c.failureCount}` : '·'}
                  </span>
                  {/* 最后活跃（超短相对时间） */}
                  <span className="w-9 shrink-0 text-right font-mono text-[10px] tabular-nums text-muted-foreground/60">
                    {hasLast ? agoShort(lastTs) : '—'}
                  </span>
                  {/* 命中脉冲：pulse 变化 → 重挂载重放一次横向扫光 */}
                  {act && act.pulse > 0 && lit && (
                    <span
                      key={act.pulse}
                      className="pointer-events-none absolute inset-y-0 left-0 w-1/3 animate-bar-sweep motion-reduce:hidden"
                      style={{
                        background: `linear-gradient(90deg, transparent, rgb(${rgb} / 0.55), transparent)`,
                      }}
                    />
                  )}
                </div>
              </TooltipTrigger>
              <TooltipContent side="right">
                <CredTooltipBody c={c} act={act} />
              </TooltipContent>
            </Tooltip>
          )
        })}
      </div>
    </TooltipProvider>
  )
}
