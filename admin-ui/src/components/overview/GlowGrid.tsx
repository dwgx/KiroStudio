import type { CredentialStatusItem } from '@/types/api'
import { Tooltip, TooltipTrigger, TooltipContent, TooltipProvider } from '@/components/ui/tooltip'
import type { CellActivity } from '@/components/overview/StatusHeatmap'
import { healthOf, HEALTH_RGB, HEALTH_LABEL, CredTooltipBody, EmptyPool } from '@/components/overview/credViz'

export interface GlowGridProps {
  credentials: CredentialStatusItem[]
  /** credential_id -> 实时活动；发现新命中时该格泛起高光扫过 + 涟漪光环（体现请求流入 / 并发）。 */
  activity?: Map<number, CellActivity>
  className?: string
}

/** 统一圆角：灯体本身与各覆盖层（扫光/辉光/涟漪/边缘流光）必须一致，否则圆角处会露边。 */
const R = 'rounded-[10px]'

/** 呼吸辉光节奏：在途号更快（像加速心跳）、有失败号略急促、健康号沉稳。 */
function glowDuration(health: 'healthy' | 'warn' | 'disabled', busy: boolean): string {
  if (busy) return '1.5s'
  if (health === 'warn') return '2.6s'
  return '3.6s'
}

/**
 * GlowGrid —— 发光网格（服务器机柜“通道指示灯墙”）。
 *
 * 质感要点：
 * - 灯体：顶部高光斑 + 基色径向渐变 + 内凹描边，像一颗嵌进面板的透镜指示灯。
 * - 常驻呼吸：双层辉光（内芯亮而稳 + 外晕缓慢扩散起伏），健康沉稳 / 有失败略急促。
 * - 命中：三重叠加——白色扫光掠过 + 内芯白闪外晕骤亮的辉光脉冲 + 自中心向外扩散的涟漪光环。
 * - 在途（inflight>0）：一道细亮弧沿边缘持续巡游（边缘流光）+ 呼吸加速，一眼看出谁在处理请求。
 * - hover：轻微放大浮起 + 外辉光增强，tooltip 展示免费字段（不含 balance，封号红线）。
 * 纯 CSS/SVG + Radix Tooltip，无图表库；motion-reduce 全面降级为静态色块。
 */
export function GlowGrid({ credentials, activity, className }: GlowGridProps) {
  if (credentials.length === 0) {
    return <EmptyPool className={className} />
  }

  return (
    <TooltipProvider delayDuration={80}>
      <div className={className}>
        <div
          className="grid gap-2.5"
          style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(36px, 1fr))' }}
        >
          {credentials.map((c) => {
            const h = healthOf(c)
            const rgb = HEALTH_RGB[h]
            const act = activity?.get(c.id)
            const lit = h !== 'disabled'
            const inflight = c.inflight ?? 0
            const busy = lit && inflight > 0
            const hit = !!act && act.pulse > 0
            return (
              <Tooltip key={c.id}>
                <TooltipTrigger asChild>
                  <div
                    className={`group relative aspect-square cursor-pointer ${R} transition-transform duration-200 ease-out-expo hover:z-10 hover:-translate-y-0.5 hover:scale-[1.14]`}
                    style={{
                      // 灯体：顶部一枚白色高光斑（透镜反光）叠在基色径向渐变上；禁用号只留哑光斜面。
                      background: lit
                        ? `radial-gradient(circle at 50% 32%, rgb(255 255 255 / 0.3), transparent 44%), radial-gradient(circle at 50% 40%, rgb(${rgb} / 0.95), rgb(${rgb} / 0.5) 62%, rgb(${rgb} / 0.28))`
                        : `linear-gradient(150deg, rgb(${rgb} / 0.5), rgb(${rgb} / 0.22))`,
                      border: `1px solid rgb(${rgb} / ${lit ? 0.55 : 0.35})`,
                      // 内凹描边 + 上缘高光 + 下缘暗角，做出“嵌进面板”的物理厚度；外辉光交给独立呼吸层。
                      boxShadow: lit
                        ? `inset 0 1px 0 rgb(255 255 255 / 0.2), inset 0 -2px 4px rgb(0 0 0 / 0.35), inset 0 0 0 1px rgb(${rgb} / 0.15)`
                        : `inset 0 1px 0 rgb(255 255 255 / 0.04), inset 0 0 8px rgb(0 0 0 / 0.45)`,
                    }}
                  >
                    {/* 常驻呼吸辉光（独立层）：双层——内芯稳 + 外晕扩散起伏；在途/有失败改节奏。 */}
                    {lit && (
                      <span
                        className={`pointer-events-none absolute inset-0 ${R} animate-idle-glow-rich motion-reduce:hidden`}
                        style={{ ['--glow-rgb' as string]: rgb, animationDuration: glowDuration(h, busy) }}
                      />
                    )}
                    {/* 灯体内层（圆角裁剪）：顶部反光斑 + 命中扫光都限制在灯体内。 */}
                    <span className={`pointer-events-none absolute inset-0 overflow-hidden ${R}`}>
                      <span className="absolute inset-x-1 top-1 h-1/3 rounded-full bg-white/15 blur-[2px] transition-opacity duration-200 group-hover:bg-white/30" />
                      {hit && (
                        <span
                          key={act!.pulse}
                          className="absolute inset-0 animate-glow-sweep bg-gradient-to-b from-white/90 via-white/25 to-transparent motion-reduce:hidden"
                        />
                      )}
                    </span>
                    {/* 在途：细亮弧沿边缘巡游（conic + border-only 遮罩），克制不刺眼。 */}
                    {busy && (
                      <span
                        className={`pointer-events-none absolute inset-0 ${R} animate-border-beam motion-reduce:hidden`}
                        style={{ ['--glow-rgb' as string]: rgb }}
                      />
                    )}
                    {/* hover 外辉光增强（透明→显现，避免常驻炫光）。 */}
                    {lit && (
                      <span
                        className={`pointer-events-none absolute inset-0 ${R} opacity-0 transition-opacity duration-200 group-hover:opacity-100 motion-reduce:transition-none`}
                        style={{ boxShadow: `0 0 18px 2px rgb(${rgb} / 0.55), 0 6px 14px rgb(0 0 0 / 0.45)` }}
                      />
                    )}
                    {/* 当前活跃：安静的品牌色描边环。 */}
                    {c.isCurrent && (
                      <span className={`pointer-events-none absolute inset-0 ${R} ring-1 ring-primary/70 ring-offset-1 ring-offset-card`} />
                    )}
                    {/* 命中辉光脉冲（不裁剪）：内芯白闪 + 外晕骤亮再衰减。 */}
                    {hit && lit && (
                      <span
                        key={`burst-${act!.pulse}`}
                        className={`pointer-events-none absolute inset-0 ${R} animate-glow-burst-rich motion-reduce:hidden`}
                        style={{ ['--glow-rgb' as string]: rgb }}
                      />
                    )}
                    {/* 命中涟漪光环（不裁剪）：自中心向外扩散一圈并淡出，“数据打进来了”的实感。 */}
                    {hit && lit && (
                      <span
                        key={`ripple-${act!.pulse}`}
                        className={`pointer-events-none absolute inset-0 ${R} animate-glow-ripple-ring motion-reduce:hidden`}
                        style={{ ['--glow-rgb' as string]: rgb }}
                      />
                    )}
                  </div>
                </TooltipTrigger>
                <TooltipContent>
                  <CredTooltipBody c={c} act={act} />
                </TooltipContent>
              </Tooltip>
            )
          })}
        </div>
        {/* 图例 */}
        <div className="mt-4 flex flex-wrap items-center gap-x-4 gap-y-1.5 text-xs text-muted-foreground">
          {(['healthy', 'warn', 'disabled'] as const).map((k) => (
            <span key={k} className="flex items-center gap-1.5">
              <span
                className="h-2.5 w-2.5 rounded-full"
                style={{
                  background: `rgb(${HEALTH_RGB[k]} / 0.9)`,
                  boxShadow: k !== 'disabled' ? `0 0 6px rgb(${HEALTH_RGB[k]} / 0.6)` : 'none',
                }}
              />
              {HEALTH_LABEL[k]}
            </span>
          ))}
          <span className="flex items-center gap-1.5">
            <span className={`h-2.5 w-2.5 rounded-full bg-transparent ring-1 ring-primary/70`} /> 当前活跃
          </span>
          <span className="flex items-center gap-1.5">
            <span
              className={`relative h-2.5 w-2.5 rounded-[4px]`}
              style={{ background: `rgb(${HEALTH_RGB.healthy} / 0.25)` }}
            >
              <span
                className={`absolute inset-0 rounded-[4px] animate-border-beam motion-reduce:hidden`}
                style={{ ['--glow-rgb' as string]: HEALTH_RGB.healthy }}
              />
            </span>
            处理中
          </span>
        </div>
      </div>
    </TooltipProvider>
  )
}
