import type { CredentialStatusItem } from '@/types/api'
import type { CellActivity } from '@/components/overview/StatusHeatmap'
import { healthOf, HEALTH_RGB, HEALTH_LABEL, EmptyPool, useHoverCard } from '@/components/overview/credViz'
import { useFlip } from '@/hooks/use-flip'
import './glow-grid.css'

export interface GlowGridProps {
  credentials: CredentialStatusItem[]
  /** credential_id -> 实时活动；发现新命中时该核心瞬间点亮再衰减并向相邻核心扩散一圈微光涟漪（体现算力被激活）。 */
  activity?: Map<number, CellActivity>
  className?: string
}

/** 统一圆角：核心本体与各覆盖层（呼吸/命中闪/涟漪）保持一致，避免圆角处露边。 */
const R = 'rounded-[4px]'

/** 呼吸周期：健康沉稳偏慢、有失败略促、在途更快（像算力负载抬升时脉动加快）。 */
function breatheDuration(health: 'healthy' | 'warn' | 'disabled', busy: boolean): string {
  if (busy) return '2s'
  if (health === 'warn') return '3s'
  return '4.4s'
}

/**
 * 由凭据 id 派生一个确定性的呼吸相位偏移（0~-1 个周期内）。
 * 让整墙核心此起彼伏地脉动而非齐刷刷同步，观感更像真实算力阵列；
 * 纯函数无随机，重渲染不跳变。
 */
function breatheDelay(id: number): string {
  // 取 id 的伪散列落到 [0,1)，映射成负延迟（动画像已运行了一段）。
  const frac = ((id * 2654435761) % 1000) / 1000
  return `-${(frac * 4).toFixed(2)}s`
}

/**
 * GlowGrid —— GPU / CUDA 核心阵列（算力墙点阵）。
 *
 * 观感要点：
 * - 阵列：小而密的方块核心规整排布，像 GPU die 上的 SM / CUDA 核心墙，一格 = 一个号。
 * - 底光：健康色作核心底光，做低频呼吸（健康沉稳 / 有失败略促 / 在途更快）；禁用号哑光无呼吸。
 * - 命中：请求流过该号时对应核心瞬间点亮成高亮（白芯 + 基色晕）再衰减，
 *   并向相邻核心扩散一圈微光涟漪 —— “算力被激活点亮”的实感。由真实 activity 事件驱动，不常驻。
 * - 当前活跃：安静的品牌色描边环。
 * - hover：轻微浮起 + 外辉光增强，tooltip 展示免费字段（不含 balance，避免触发上游风控）。
 * 纯 CSS/SVG + Radix Tooltip，无图表库；命中/涟漪用 key 重挂载单次重放（不常驻）；
 * 常驻仅低频呼吸（opacity）；数百核心不卡；motion-reduce 全面降级为静态色块。
 */
export function GlowGrid({ credentials, activity, className }: GlowGridProps) {
  // 鼠标跟随悬浮卡（替代 Radix Tooltip 固定 side 的边缘翻转，卡片黏着鼠标走）。
  const hoverCard = useHoverCard()
  // FLIP 平滑重排:排序/显隐变化时核心从旧位滑到新位。
  const flipRef = useFlip<HTMLDivElement>([credentials.map((c) => c.id).join(',')])

  if (credentials.length === 0) {
    return <EmptyPool className={className} />
  }

  return (
    <div className={className}>
        <div
          ref={flipRef}
          className="grid gap-1.5"
          style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(22px, 1fr))' }}
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
                  <div
                    key={c.id}
                    data-flip-key={c.id}
                    onMouseEnter={(e) => hoverCard.show(c, e)}
                    onMouseMove={hoverCard.move}
                    onMouseLeave={hoverCard.hide}
                    className={`group relative aspect-square cursor-pointer ${R} transition-transform duration-200 ease-out hover:z-10 hover:-translate-y-0.5 hover:scale-[1.18]`}
                    style={{
                      // 核心本体：基色斜面渐变 + 内凹描边，像 die 上一枚微小的算力单元。
                      // 底光的“亮”交给独立呼吸层调 opacity（GPU 合成，不重绘本体）。
                      background: lit
                        ? `linear-gradient(150deg, rgb(${rgb} / 0.42), rgb(${rgb} / 0.16))`
                        : `linear-gradient(150deg, rgb(${rgb} / 0.45), rgb(${rgb} / 0.2))`,
                      border: `1px solid rgb(${rgb} / ${lit ? 0.5 : 0.32})`,
                      boxShadow: lit
                        ? `inset 0 1px 0 rgb(255 255 255 / 0.14), inset 0 -1px 2px rgb(0 0 0 / 0.35)`
                        : `inset 0 1px 0 rgb(255 255 255 / 0.03), inset 0 0 6px rgb(0 0 0 / 0.45)`,
                    }}
                  >
                    {/* 常驻底光呼吸（独立层，只动 opacity）：健康色内发光低频起伏；禁用号无呼吸。 */}
                    {lit && (
                      <span
                        className={`gg-core-breathe pointer-events-none absolute inset-0 ${R}`}
                        style={{
                          background: `radial-gradient(circle at 50% 42%, rgb(${rgb} / 0.9), rgb(${rgb} / 0.32) 70%, transparent)`,
                          boxShadow: `0 0 6px 0 rgb(${rgb} / 0.5)`,
                          ['--gg-dur' as string]: breatheDuration(h, busy),
                          ['--gg-delay' as string]: breatheDelay(c.id),
                        }}
                      />
                    )}
                    {/* 顶部一枚白色高光斑（透镜反光），hover 时加强，做出核心的物理厚度。 */}
                    <span className={`pointer-events-none absolute inset-0 overflow-hidden ${R}`}>
                      <span className="absolute inset-x-0.5 top-0.5 h-1/3 rounded-full bg-white/12 blur-[1.5px] transition-opacity duration-200 group-hover:bg-white/28" />
                    </span>
                    {/* hover 外辉光增强（透明→显现，避免常驻炫光）。 */}
                    {lit && (
                      <span
                        className={`pointer-events-none absolute inset-0 ${R} opacity-0 transition-opacity duration-200 group-hover:opacity-100 motion-reduce:transition-none`}
                        style={{ boxShadow: `0 0 12px 1px rgb(${rgb} / 0.6), 0 4px 10px rgb(0 0 0 / 0.45)` }}
                      />
                    )}
                    {/* 当前活跃：安静的品牌色描边环。 */}
                    {c.isCurrent && (
                      <span className={`pointer-events-none absolute inset-0 ${R} ring-1 ring-primary/70 ring-offset-1 ring-offset-card`} />
                    )}
                    {/* 命中点亮闪（不裁剪）：核心被算力激活，白芯 + 基色晕瞬间点亮再衰减，单次重放。 */}
                    {hit && lit && (
                      <span
                        key={`flash-${act!.pulse}`}
                        className={`gg-core-flash pointer-events-none absolute inset-0 ${R} motion-reduce:hidden`}
                        style={{
                          background: `radial-gradient(circle at 50% 42%, rgb(255 255 255 / 0.95), rgb(${rgb} / 0.6) 60%, transparent)`,
                          boxShadow: `0 0 14px 2px rgb(${rgb} / 0.85)`,
                        }}
                      />
                    )}
                    {/* 命中涟漪微光（不裁剪）：自本核心向外扩散一圈细描边并淡出，波及相邻核心。 */}
                    {hit && lit && (
                      <span
                        key={`ripple-${act!.pulse}`}
                        className={`gg-core-ripple pointer-events-none absolute inset-0 ${R} motion-reduce:hidden`}
                        style={{ boxShadow: `0 0 0 1px rgb(${rgb} / 0.9)` }}
                      />
                    )}
                  </div>
            )
          })}
        </div>
        {/* 图例 */}
        <div className="mt-4 flex flex-wrap items-center gap-x-4 gap-y-1.5 text-xs text-muted-foreground">
          {(['healthy', 'warn', 'disabled'] as const).map((k) => (
            <span key={k} className="flex items-center gap-1.5">
              <span
                className="h-2.5 w-2.5 rounded-[3px]"
                style={{
                  background: `rgb(${HEALTH_RGB[k]} / 0.9)`,
                  boxShadow: k !== 'disabled' ? `0 0 6px rgb(${HEALTH_RGB[k]} / 0.6)` : 'none',
                }}
              />
              {HEALTH_LABEL[k]}
            </span>
          ))}
          <span className="flex items-center gap-1.5">
            <span className={`h-2.5 w-2.5 rounded-[3px] bg-transparent ring-1 ring-primary/70`} /> 当前活跃
          </span>
          <span className="flex items-center gap-1.5">
            <span
              className="relative h-2.5 w-2.5 rounded-[3px]"
              style={{ background: `rgb(${HEALTH_RGB.healthy} / 0.25)` }}
            >
              <span
                className="gg-core-breathe absolute inset-0 rounded-[3px]"
                style={{
                  background: `radial-gradient(circle at 50% 42%, rgb(${HEALTH_RGB.healthy} / 0.9), transparent 70%)`,
                  ['--gg-dur' as string]: '2s',
                }}
              />
            </span>
            处理中
          </span>
        </div>
      {/* 鼠标跟随悬浮卡（正文 CredTooltipBody 不变，仅定位改为黏鼠标） */}
      {hoverCard.render((id) => activity?.get(id))}
      </div>
  )
}
