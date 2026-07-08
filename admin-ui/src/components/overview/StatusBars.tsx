import { useMemo } from 'react'
import type { CredentialStatusItem, CachedBalanceItem } from '@/types/api'
import { Tooltip, TooltipTrigger, TooltipContent, TooltipProvider } from '@/components/ui/tooltip'
import type { CellActivity } from '@/components/overview/StatusHeatmap'
import { healthOf, HEALTH_RGB, CredTooltipBody, EmptyPool } from '@/components/overview/credViz'
import { useCachedBalances } from '@/hooks/use-credentials'
import { subscriptionLabel } from '@/lib/i18n-labels'
import { X } from 'lucide-react'
import { FireCanvas } from '@/components/overview/FireCanvas'
import './status-bars.css'

export interface StatusBarsProps {
  credentials: CredentialStatusItem[]
  /** credential_id -> 实时活动；来请求时对应条闪一下。 */
  activity?: Map<number, CellActivity>
  /**
   * 火力全开(RPM 饱和)的凭据 id 集合——由概览页从 /ratelimit/insights 的 rpmSaturated 派生传入。
   * 命中的号条会点燃 effort-card 风格的 WebGL 火焰(条件挂载,同时通常仅 1-2 个,数百号不崩)。
   * 不传则不点火(优雅降级)。
   */
  saturatedIds?: Set<number>
  /**
   * 可选：id(字符串) -> 已缓存余额快照。由概览页 useCachedBalances 传入即可全页共享一份。
   * 不传时本组件自行订阅缓存端点（react-query 去重，不产生额外请求）。
   * 只读缓存，绝不触发上游（避免触发上游风控）。
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

// ── 算力格子阵列可调常量 ──（合并版：凌晨观感 + 算法加减；参数集中此处便于调）
// 窄条(84×16px)上排 COLS×ROWS 个小核心，像 GPU die 上的 SM 阵列。
const GRID_COLS = 14
const GRID_ROWS = 3
const GRID_CELLS = GRID_COLS * GRID_ROWS
// 点亮格数 = round(利用率 × 总格数)（算法加减：量化成整数格，负载升降时格子沿固定次序一颗颗加/减）。
const MIN_LIT_WHEN_ACTIVE = 1
// 加/减一颗格子的亮灭过渡时长(ms)：新格点亮/旧格熄灭平滑淡入淡出，不硬跳。
const CELL_TRANSITION_MS = 420
// 逐格错峰步进(ms/格)：加减沿“到填充前沿的距离”像涟漪依次发生，而非齐刷刷同时亮灭。
const CELL_STAGGER_MS = 26
// 错峰总延迟上限(ms)：远端格子最多滞后这么久。
const CELL_STAGGER_CAP_MS = 520
// 黄金比低差异序列常数：给格子排一个“散布均匀”的填充次序（负载升时匀称铺满整片 die）。
const GOLDEN = 0.6180339887498949

// 火焰强度 0..1:由 RPM 与在途综合派生(RPM 60/min 视为满档,在途 6 视为满档,取较大者)。
// 驱动 FireCanvas 配色分级——越猛越偏 Ruby 红(全满=最强),否则青/绿/橙/紫渐变。
function fireIntensity(rpm: number, inflight: number): number {
  const r = Math.min(rpm / 60, 1)
  const i = Math.min(inflight / 6, 1)
  return Math.max(r, i)
}

/**
 * CellFlow —— 条内一小片 GPU/CUDA 算力格子阵列 = 实时算力利用率表（dwgx：凌晨格子观感 + 算法加减，融合版）。
 *
 * 【合并要点】
 * - **算法加减**（取自后一版）：点亮格数 litCount = round(利用率 × 总格数)，格子按**黄金比填充序**
 *   固定次序取前 litCount 个点亮 → 负载升降时格子一颗颗匀称地加/减，并按“到填充前沿距离”错峰过渡（涟漪）。
 * - **凌晨观感**（取自凌晨版）：熄灭格**只压暗、不缩小**（整片 die 始终满格，无“缩成小点”的空洞感）；
 *   点亮格保留 opacity 微闪（相位/速度/深度各异，此起彼伏不齐刷刷）。
 * ① 利用率 level(0..1) 由真实 RPM/在途派生：决定点亮多少格、闪动多快。
 * ② 每个真实请求（pulse）→ 一道**点火波**自左向右穿过整阵（overlay 按列错峰 ignite，key=pulse 单次重放）。
 * ③ 空闲/禁用：暗格静止零动画（GPU 不空转）。火力全开(onFire)：整容器换成 WebGL 火焰，保持现状不动。
 * 全走 opacity/transform（GPU 合成，keyframes 见 status-bars.css）；motion-reduce 退化为静态占用格。
 */
function CellFlow({
  rgb,
  rpm,
  inflight,
  pulse,
  litHealth,
  onFire,
}: {
  rgb: string
  rpm: number
  inflight: number
  pulse: number
  litHealth: boolean
  onFire: boolean
}) {
  // 火力全开(自动挡):同一个 RPM 容器直接拉满成 effort-card WebGL 火焰(条件挂载,只有饱满号有上下文)。
  if (onFire) {
    return (
      <div
        className="relative flex h-4 w-[84px] shrink-0 items-center overflow-hidden rounded-[2px]"
        title={`火力全开（RPM ${rpm} · 在途 ${inflight}）`}
        aria-hidden
      >
        {/* 火焰强度由 RPM/在途派生:越猛越偏 Ruby 红(满档最强);青→绿→金→橙→紫→白热→Ruby 红 7 档连续过渡 */}
        <FireCanvas active intensity={fireIntensity(rpm, inflight)} className="absolute inset-0" />
      </div>
    )
  }
  // 算力利用率 0..1：RPM 为主（40 rpm 视为满占用），在途作为下限托底（有在途至少点亮小片）。
  const rpmLevel = Math.min(rpm / 40, 1)
  const inflightLevel = Math.min(inflight / 3, 1)
  const level = litHealth ? Math.max(rpmLevel, inflightLevel * 0.5) : 0
  const active = litHealth && level > 0

  // 每格的确定性静态参数（随 GRID_CELLS 生成一次，稳定）：
  // - order：黄金比低差异填充名次（散布均匀）——litCount 个格子总取名次最小的前 litCount 个点亮，
  //   负载升降时格子沿此次序一颗颗加/减（算法加减），不随机跳。
  // - phase/speed/depth：点亮后 opacity 微闪的相位/速度/深度扰动 → 整阵此起彼伏而非齐闪（凌晨观感）。
  const cells = useMemo(() => {
    const keyed = Array.from({ length: GRID_CELLS }, (_, i) => {
      const k = ((i + 1) * GOLDEN) % 1 // 填充序 key
      const seed2 = Math.sin((i + 1) * 78.233) * 12543.1234
      const r2 = seed2 - Math.floor(seed2)
      const seed3 = Math.sin((i + 1) * 39.111) * 27183.845
      const r3 = seed3 - Math.floor(seed3)
      return { i, k, r2, r3 }
    })
    const bySpread = [...keyed].sort((a, b) => a.k - b.k)
    const order = new Array<number>(GRID_CELLS)
    bySpread.forEach((c, rank) => (order[c.i] = rank))
    return keyed.map((c) => ({
      order: order[c.i],
      phase: -(c.r2 * 1.2).toFixed(3),
      speed: 0.72 + c.r2 * 0.5,
      depth: 0.35 + c.r3 * 0.4,
    }))
  }, [])

  // 点亮格数：利用率量化成整数格（活跃时至少 MIN_LIT_WHEN_ACTIVE 个 → 有“心跳”）。
  const litCount = active
    ? Math.max(MIN_LIT_WHEN_ACTIVE, Math.round(level * GRID_CELLS))
    : 0
  // 由利用率派生闪动速度：越忙闪得越快（0.35s 满档 ~ 0.95s 轻载）。
  const flickerDur = 0.35 + (1 - level) * 0.6
  // 请求点火波：整层单次重放，时长随负载略变（越忙越快）。
  const igniteDur = 0.42 + (1 - level) * 0.28
  // 每列点火错峰步进：让点火波从左列到右列依次亮起，穿过整阵。
  const igniteStep = igniteDur * 0.55 / GRID_COLS

  return (
    <div
      className="relative grid h-4 w-[84px] shrink-0 gap-[1.5px] overflow-hidden rounded-[2px]"
      style={{ gridTemplateColumns: `repeat(${GRID_COLS}, 1fr)`, gridTemplateRows: `repeat(${GRID_ROWS}, 1fr)` }}
      title={active ? `算力利用率 ${(level * 100).toFixed(0)}%（${litCount}/${GRID_CELLS} 核 · RPM ${rpm} · 在途 ${inflight}）` : '空闲'}
      aria-hidden
    >
      {cells.map((c, i) => {
        // 是否点亮：名次落在前 litCount 名内（算法加减：litCount 增减 → 沿固定填充序一颗颗加/减）。
        const on = c.order < litCount
        // 到填充前沿的距离 → 加减错峰 delay，让一颗颗加/减像涟漪从前沿扩散。
        const stagger = Math.min(Math.abs(c.order - litCount) * CELL_STAGGER_MS, CELL_STAGGER_CAP_MS)
        // 点亮格闪动的高/低 opacity：越忙峰值越高、谷越浅（更饱满）。
        const hi = on ? Math.min(0.55 + level * 0.45, 1) : 0.12
        const lo = on ? Math.max(hi * c.depth, 0.2) : 0.12
        return (
          <span
            key={i}
            className={`sbar-cell ${on ? 'sbar-cell-run motion-reduce:animate-none' : ''}`}
            style={{
              // 凌晨观感：熄灭格只压暗、不缩小（整片 die 始终满格，无空洞感）。
              background: on
                ? `rgb(${rgb} / 0.95)`
                : `rgb(${rgb} / ${active ? 0.14 : 0.1})`,
              boxShadow: on ? `0 0 3px rgb(${rgb} / 0.55)` : 'none',
              opacity: on ? undefined : lo,
              // 加减时的亮灭过渡（仅淡入淡出，无 scale）+ 按前沿距离错峰。
              transitionDuration: `${CELL_TRANSITION_MS}ms`,
              transitionDelay: `${stagger}ms`,
              ['--cell-lo' as string]: lo.toFixed(3),
              ['--cell-hi' as string]: hi.toFixed(3),
              animationDuration: on ? `${(flickerDur * c.speed).toFixed(3)}s` : undefined,
              animationDelay: on ? `${c.phase}s` : undefined,
            }}
          />
        )
      })}
      {/* 真实请求点火波：整层重挂载(key=pulse)，每列错峰 ignite → 一道点火自左向右穿过算力阵。
          每列一条竖条(跨全部行)，按列 index 递增 animation-delay，形成左→右扫过的点火波。 */}
      {active && pulse > 0 &&
        Array.from({ length: GRID_COLS }, (_, col) => (
          <span
            key={`ignite-${pulse}-${col}`}
            className="sbar-cell-ignite pointer-events-none motion-reduce:hidden"
            style={{
              gridColumn: col + 1,
              gridRow: `1 / span ${GRID_ROWS}`,
              background: `rgb(${rgb} / 0.98)`,
              boxShadow: `0 0 4px rgb(${rgb} / 0.85)`,
              ['--ignite-dur' as string]: `${igniteDur.toFixed(2)}s`,
              animationDelay: `${(col * igniteStep).toFixed(3)}s`,
            }}
          />
        ))}
    </div>
  )
}

/**
 * StatusBars —— 横向状态条带（工程感、信息密度高，运维面板风格）。
 *
 * 每个号一条细横条：左侧健康色带 + 健康灯 + #id + 邮箱(无则“无邮箱”占位) + 订阅等级，
 * 右侧一排紧凑指标：数据流小格子(活跃度) / 余额迷你进度条(剩余%) / RPM / 在途 / 成功·失败 / 最后活跃。
 * 无 email 的号不再留空占位（旧 bug：整条空荡荡），改由这排指标把条体填满、右对齐。
 * 余额只读缓存端点（零上游、不封号），拿不到就优雅省略该格并保留等宽占位以维持列对齐。
 * 每号条内嵌一排小方格：能量顺着格子往右流，亮格段长/流速由真实 RPM 与在途驱动（越忙越多越快，
 * 空闲暗且静止）；命中时一道更亮的流掠过整排格子。当前活跃号常驻高亮环。
 * 纯 CSS transform/opacity（GPU 合成）+ Radix Tooltip，数百号不常驻数百动画，motion-reduce 降级。
 */
export function StatusBars({ credentials, activity, balances, saturatedIds, className }: StatusBarsProps) {
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
          // 火力全开(触发率放宽,dwgx 要多能看到):非禁用号,满足任一即点火——
          // ①后端判定 RPM 饱和(saturatedIds,含 rpm_limit=0 时的高水位兜底)
          // ②在途 ≥2(并发打满)③RPM ≥20(打得猛)。任一即拉满成 WebGL 火焰。
          const onFire = lit && (!!saturatedIds?.has(c.id) || inflight >= 2 || rpm >= 20)

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
                    className="pointer-events-none absolute inset-y-0 left-0 w-[3px] z-[1]"
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
                  {/* 号标识（火力全开由 RPM 容器内的 WebGL 火焰表达，#id 前不再放火苗图标） */}
                  <span className="relative z-[1] flex shrink-0 items-center font-mono text-xs tabular-nums text-foreground">
                    #{c.id}
                  </span>
                  {/* 别名优先 > 邮箱 > 无邮箱占位。设了别名(name)就显别名(如"#53"),不再强显长邮箱。 */}
                  <span
                    className={`min-w-0 flex-1 truncate text-xs ${
                      c.name || c.email ? 'text-muted-foreground' : 'italic text-muted-foreground/40'
                    }`}
                  >
                    {c.name || c.email || '无邮箱'}
                  </span>
                  {/* 订阅等级：固定等宽槽位（无论有无都占位），保证各行 KIRO POWER 标签对齐不跳动 */}
                  <span className="hidden w-[76px] shrink-0 sm:block">
                    {sub && (
                      <span className="inline-block max-w-full truncate rounded bg-secondary/70 px-1.5 py-0.5 font-mono text-[9px] uppercase tracking-wide text-muted-foreground/90">
                        {subscriptionLabel(sub)}
                      </span>
                    )}
                  </span>
                  {/* RPM 活跃度容器（自动挡）：未饱满=负载格子流；火力全开(onFire)=同一容器自动拉满成 WebGL 火焰 */}
                  <CellFlow
                    rgb={rgb}
                    rpm={rpm}
                    inflight={inflight}
                    pulse={act?.pulse ?? 0}
                    litHealth={lit}
                    onFire={onFire}
                  />
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
                  {/* RPM（近 60s）：固定等宽展位，有才显徽标、无则空占位 → 保证后续各列所有行对齐不跳 */}
                  <span className="flex w-[46px] shrink-0 justify-end" aria-hidden={!(rpm > 0)}>
                    {rpm > 0 && (
                      <span className="rounded bg-sky-500/10 px-1.5 py-0.5 font-mono text-[10px] tabular-nums text-sky-300/90">
                        {rpm}
                        <span className="text-[8px] text-sky-300/60">/m</span>
                      </span>
                    )}
                  </span>
                  {/* 在途徽标：固定等宽展位（同上，dwgx 反复提的"预留 UI 展位"，避免徽标有无导致列位移） */}
                  <span className="flex w-[52px] shrink-0 justify-end" aria-hidden={!(inflight > 0)}>
                    {inflight > 0 && (
                      <span className="rounded bg-primary/15 px-1.5 py-0.5 font-mono text-[10px] tabular-nums text-primary">
                        在途 {inflight}
                      </span>
                    )}
                  </span>
                  {/* 成功 / 失败计数（等宽右对齐） */}
                  <span className="w-10 shrink-0 text-right font-mono text-xs tabular-nums text-emerald-400/90">
                    {c.successCount}
                  </span>
                  <span
                    className={`w-8 shrink-0 text-right font-mono text-xs tabular-nums ${
                      c.failureCount > 0 ? 'text-red-400/80' : 'text-muted-foreground/25'
                    }`}
                  >
                    {c.failureCount > 0 ? (
                      <span className="inline-flex items-center justify-end">
                        <X className="h-3 w-3" />
                        {c.failureCount}
                      </span>
                    ) : (
                      '·'
                    )}
                  </span>
                  {/* 最后活跃（超短相对时间） */}
                  <span className="w-9 shrink-0 text-right font-mono text-[10px] tabular-nums text-muted-foreground/60">
                    {hasLast ? agoShort(lastTs) : '—'}
                  </span>
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
