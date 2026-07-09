import { useMemo } from 'react'
import type { CredentialStatusItem } from '@/types/api'
import type { CellActivity } from '@/components/overview/StatusHeatmap'
import { healthOf, HEALTH_RGB, EmptyPool, useHoverCard } from '@/components/overview/credViz'

export interface OrbitRingProps {
  credentials: CredentialStatusItem[]
  /** credential_id -> 实时活动；活跃时节点亮起涟漪 + 与中心的连线泛光。 */
  activity?: Map<number, CellActivity>
  className?: string
}

const VIEWBOX = 320
const CENTER = VIEWBOX / 2
const NODE_R = 6.5 // 节点半径
const RING_GAP = 46 // 环间距
const BASE_R = 58 // 最内环半径
const MAX_PER_RING = 14 // 单环最多节点数，超出分到外环

interface PlacedNode {
  c: CredentialStatusItem
  x: number
  y: number
  ring: number
}

/** 把凭据分配到同心环上：内环先排满再溢出到外环，每环均匀分布。 */
function placeNodes(creds: CredentialStatusItem[]): { nodes: PlacedNode[]; rings: number } {
  const nodes: PlacedNode[] = []
  let idx = 0
  let ring = 0
  while (idx < creds.length) {
    // 越外环容量越大（周长更长），随环号线性放大
    const cap = MAX_PER_RING + ring * 6
    const count = Math.min(cap, creds.length - idx)
    const radius = BASE_R + ring * RING_GAP
    for (let i = 0; i < count; i++) {
      // 起始角错开半格，避免各环节点径向对齐显得死板
      const angle = (i / count) * Math.PI * 2 - Math.PI / 2 + (ring % 2 ? Math.PI / count : 0)
      nodes.push({
        c: creds[idx + i],
        x: CENTER + Math.cos(angle) * radius,
        y: CENTER + Math.sin(angle) * radius,
        ring,
      })
    }
    idx += count
    ring++
  }
  return { nodes, rings: ring }
}

/**
 * OrbitRing —— 环形/星座布局。
 *
 * 号排成一个或多个同心环（多了自动分环），每个号一个发光节点。
 * 中心显示“可用 / 总数”。活跃号亮起一圈扩散涟漪，并与中心泛起一条脉冲连线，
 * 像调度中心把请求派发到某个节点。走克制高级暗色 + 精准发光，无紫粉赛博俗气。
 * 纯 SVG + Radix Tooltip，无图表库；motion-reduce 下去掉涟漪/脉冲，仅保留静态节点。
 */
export function OrbitRing({ credentials, activity, className }: OrbitRingProps) {
  const { nodes, rings } = useMemo(() => placeNodes(credentials), [credentials])
  // 鼠标跟随悬浮卡（替代 Radix Tooltip 固定 side 的边缘翻转，卡片黏着鼠标走）。
  const hoverCard = useHoverCard()

  if (credentials.length === 0) {
    return <EmptyPool className={className} />
  }

  const available = credentials.filter((c) => !c.disabled).length

  return (
      <div className={className}>
        <div className="relative mx-auto aspect-square w-full max-w-[380px]">
          <svg viewBox={`0 0 ${VIEWBOX} ${VIEWBOX}`} className="h-full w-full overflow-visible">
            {/* 同心轨道环（极淡的引导线） */}
            {Array.from({ length: rings }).map((_, r) => (
              <circle
                key={r}
                cx={CENTER}
                cy={CENTER}
                r={BASE_R + r * RING_GAP}
                fill="none"
                stroke="rgb(255 255 255 / 0.06)"
                strokeWidth={1}
              />
            ))}

            {/* 活跃节点 → 中心的脉冲连线（先画在节点下方） */}
            {nodes.map(({ c, x, y }) => {
              const act = activity?.get(c.id)
              if (!act || act.pulse === 0 || c.disabled) return null
              return (
                <line
                  key={`${c.id}-${act.pulse}`}
                  x1={CENTER}
                  y1={CENTER}
                  x2={x}
                  y2={y}
                  stroke={`rgb(${HEALTH_RGB[healthOf(c)]})`}
                  strokeWidth={1.5}
                  strokeLinecap="round"
                  className="animate-orbit-link motion-reduce:hidden"
                />
              )
            })}

            {/* 节点 */}
            {nodes.map(({ c, x, y }) => {
              const h = healthOf(c)
              const rgb = HEALTH_RGB[h]
              const act = activity?.get(c.id)
              const lit = h !== 'disabled'
              return (
                    <g
                      key={c.id}
                      className="cursor-pointer"
                      onMouseEnter={(e) => hoverCard.show(c, e)}
                      onMouseMove={hoverCard.move}
                      onMouseLeave={hoverCard.hide}
                    >
                      {/* 命中涟漪：pulse 变化重挂载，向外扩散淡出 */}
                      {act && act.pulse > 0 && lit && (
                        <circle
                          key={act.pulse}
                          cx={x}
                          cy={y}
                          r={NODE_R}
                          fill="none"
                          stroke={`rgb(${rgb})`}
                          strokeWidth={1.5}
                          className="animate-orbit-ripple motion-reduce:hidden"
                        />
                      )}
                      {/* 当前活跃：安静的品牌色外环 */}
                      {c.isCurrent && (
                        <circle
                          cx={x}
                          cy={y}
                          r={NODE_R + 3.5}
                          fill="none"
                          stroke="hsl(var(--primary) / 0.7)"
                          strokeWidth={1.25}
                        />
                      )}
                      {/* 节点本体：发光号带外辉光，禁用号暗哑 */}
                      <circle
                        cx={x}
                        cy={y}
                        r={NODE_R}
                        fill={`rgb(${rgb} / ${lit ? 0.95 : 0.5})`}
                        stroke={`rgb(${rgb} / ${lit ? 0.9 : 0.4})`}
                        strokeWidth={1}
                        className="transition-[r] duration-200 ease-out-expo hover:[r:8.5px]"
                        style={
                          lit
                            ? { filter: `drop-shadow(0 0 4px rgb(${rgb} / 0.75))` }
                            : undefined
                        }
                      />
                      {/* 健康号中心高光点 */}
                      {lit && <circle cx={x} cy={y - 1.5} r={1.6} fill="rgb(255 255 255 / 0.7)" />}
                    </g>
              )
            })}
          </svg>

          {/* 中心计数（HTML 覆盖在 SVG 上，字体渲染更清晰） */}
          <div className="pointer-events-none absolute inset-0 flex flex-col items-center justify-center">
            <span className="text-2xl font-semibold tabular-nums text-foreground">
              {available}
              <span className="text-base text-muted-foreground"> / {credentials.length}</span>
            </span>
            <span className="mt-0.5 text-[11px] uppercase tracking-wider text-muted-foreground">
              可用 / 总数
            </span>
          </div>
        </div>
      {/* 鼠标跟随悬浮卡（正文 CredTooltipBody 不变，仅定位改为黏鼠标） */}
      {hoverCard.render((id) => activity?.get(id))}
      </div>
  )
}
