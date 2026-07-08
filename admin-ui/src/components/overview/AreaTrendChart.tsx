import { useMemo, useRef, useState, useEffect, useLayoutEffect } from 'react'
import type { SeriesPoint } from '@/types/api'
import { useUsageThroughput } from '@/hooks/use-usage'

export interface AreaTrendChartProps {
  points: SeriesPoint[]
  /** 高度（px），宽度撑满容器 */
  height?: number
  /** 叠加一条淡色成功率副线（右侧 0~100% 语义），让单图承载“请求量 + 质量”两维 */
  showRate?: boolean
  /** x 轴刻度格式：小时视图显示 MM/DD HH:00，天视图显示 MM/DD */
  granularity?: 'hourly' | 'daily'
  className?: string
}

const PAD_T = 16
const PAD_B = 24
const PAD_X = 6

type Pt = { x: number; y: number }
type Seg = { p1: Pt; cp1: Pt; cp2: Pt; p2: Pt }

function fmtHour(ts: number): string {
  const d = new Date(ts)
  return `${String(d.getMonth() + 1).padStart(2, '0')}/${String(d.getDate()).padStart(2, '0')} ${String(d.getHours()).padStart(2, '0')}:00`
}

function fmtDay(ts: number): string {
  const d = new Date(ts)
  return `${String(d.getMonth() + 1).padStart(2, '0')}/${String(d.getDate()).padStart(2, '0')}`
}

// 紧凑数字：1234 → 1.2k，用于坐标轴数值标注
function compactNum(v: number): string {
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(1).replace(/\.0$/, '')}M`
  if (v >= 1_000) return `${(v / 1_000).toFixed(1).replace(/\.0$/, '')}k`
  return String(Math.round(v))
}

// Catmull-Rom → 三次贝塞尔：返回每段的控制点，既能拼路径字符串，也能按 x 采样曲线
function buildSegments(pts: Pt[]): Seg[] {
  const segs: Seg[] = []
  for (let i = 0; i < pts.length - 1; i++) {
    const p0 = pts[i - 1] ?? pts[i]
    const p1 = pts[i]
    const p2 = pts[i + 1]
    const p3 = pts[i + 2] ?? p2
    segs.push({
      p1,
      cp1: { x: p1.x + (p2.x - p0.x) / 6, y: p1.y + (p2.y - p0.y) / 6 },
      cp2: { x: p2.x - (p3.x - p1.x) / 6, y: p2.y - (p3.y - p1.y) / 6 },
      p2,
    })
  }
  return segs
}

function segsToPath(pts: Pt[], segs: Seg[]): string {
  if (pts.length === 0) return ''
  if (pts.length === 1) return `M ${pts[0].x},${pts[0].y}`
  let d = `M ${pts[0].x.toFixed(1)},${pts[0].y.toFixed(1)}`
  for (const s of segs) {
    d += ` C ${s.cp1.x.toFixed(1)},${s.cp1.y.toFixed(1)} ${s.cp2.x.toFixed(1)},${s.cp2.y.toFixed(1)} ${s.p2.x.toFixed(1)},${s.p2.y.toFixed(1)}`
  }
  return d
}

// 三次贝塞尔在参数 t 处的坐标分量
function bezAt(a: number, b: number, c: number, d: number, t: number): number {
  const mt = 1 - t
  return mt * mt * mt * a + 3 * mt * mt * t * b + 3 * mt * t * t * c + t * t * t * d
}

/**
 * 把整条平滑曲线密采样成折线点集（默认每段 SUB 个子样本）。
 * 采样点的 x 在整条曲线上单调递增（因数据点 x 等距、控制点也随之单调），
 * 便于后续用鼠标 x 做“落在曲线上”的连续插值。
 */
function sampleCurve(segs: Seg[], sub = 14): Pt[] {
  if (segs.length === 0) return []
  const out: Pt[] = [{ x: segs[0].p1.x, y: segs[0].p1.y }]
  for (const s of segs) {
    for (let k = 1; k <= sub; k++) {
      const t = k / sub
      out.push({
        x: bezAt(s.p1.x, s.cp1.x, s.cp2.x, s.p2.x, t),
        y: bezAt(s.p1.y, s.cp1.y, s.cp2.y, s.p2.y, t),
      })
    }
  }
  return out
}

/** 给定鼠标 x，在密采样折线里线性插值出曲线上的 y（圆点“吸附曲线滑行”的核心） */
function yAtX(samples: Pt[], mx: number): number {
  if (samples.length === 0) return 0
  if (mx <= samples[0].x) return samples[0].y
  const last = samples[samples.length - 1]
  if (mx >= last.x) return last.y
  // 二分定位所在子区间（samples.x 单调递增）
  let lo = 0
  let hi = samples.length - 1
  while (hi - lo > 1) {
    const mid = (lo + hi) >> 1
    if (samples[mid].x <= mx) lo = mid
    else hi = mid
  }
  const a = samples[lo]
  const b = samples[hi]
  const span = b.x - a.x || 1
  const r = (mx - a.x) / span
  return a.y + (b.y - a.y) * r
}

function clamp01(v: number): number {
  return v < 0 ? 0 : v > 1 ? 1 : v
}

// 弧长参数化表：把密采样折线转成「累计弧长」，让粒子按真实曲线长度匀速滑行
// （否则陡峭段像素跨度大、粒子会忽快忽慢）。total 为整条曲线像素长度。
type ArcTable = { pts: Pt[]; cum: number[]; total: number }
function buildArcTable(samples: Pt[]): ArcTable {
  const pts = samples
  const cum: number[] = new Array(pts.length)
  cum[0] = 0
  for (let i = 1; i < pts.length; i++) {
    const dx = pts[i].x - pts[i - 1].x
    const dy = pts[i].y - pts[i - 1].y
    cum[i] = cum[i - 1] + Math.hypot(dx, dy)
  }
  return { pts, cum, total: cum[cum.length - 1] || 0 }
}

// 给定弧长 d（0~total），二分定位落在曲线上的坐标（线性插值）
function pointAtDist(t: ArcTable, d: number): Pt {
  const { pts, cum, total } = t
  if (pts.length === 0) return { x: 0, y: 0 }
  if (pts.length === 1 || total === 0) return pts[0]
  const dd = d <= 0 ? 0 : d >= total ? total : d
  let lo = 0
  let hi = cum.length - 1
  while (hi - lo > 1) {
    const mid = (lo + hi) >> 1
    if (cum[mid] <= dd) lo = mid
    else hi = mid
  }
  const span = cum[hi] - cum[lo] || 1
  const r = (dd - cum[lo]) / span
  return { x: pts[lo].x + (pts[hi].x - pts[lo].x) * r, y: pts[lo].y + (pts[hi].y - pts[lo].y) * r }
}

const MAX_PARTICLES = 20

/**
 * 沿趋势曲线流动的发光粒子层（纯 SVG，imperative rAF 直写属性，零 React 重渲染/帧）。
 *
 * 由实时吞吐驱动：
 * - `rps`（请求/秒）→ 粒子**数量**（密度）：空闲 2 颗稀疏，繁忙最多 {@link MAX_PARTICLES} 颗。
 * - `tokensPerSec` + `rps` → 粒子**流速**与**亮度/大小**：越忙越快越亮越大。
 *
 * 每颗粒子 = 沿曲线的一段拖尾 path（glow 层，高斯模糊）+ 光晕圆 + 明亮核心圆（core 层，不模糊）。
 * 位置用弧长参数化匀速前进，到尾端回绕到头端循环（首尾各 6% 淡入淡出，回绕不闪烁）。
 * 目标密度/速度/亮度用指数平滑逐帧逼近（tau≈0.9s），吞吐变化时平滑过渡不突变。
 * motion-reduce：不跑动画，渲染少量静态淡点。tab 隐藏时 rAF 天然暂停。
 */
function FlowParticles({
  samples,
  rps,
  tokensPerSec,
}: {
  samples: Pt[]
  rps: number
  tokensPerSec: number
}) {
  const gid = useMemo(() => `flow-${Math.random().toString(36).slice(2, 8)}`, [])
  const reduce = useMemo(
    () =>
      typeof window !== 'undefined' &&
      typeof window.matchMedia === 'function' &&
      window.matchMedia('(prefers-reduced-motion: reduce)').matches,
    [],
  )

  const arc = useMemo(() => buildArcTable(samples), [samples])

  // 最新目标值放 ref，rAF 循环直接读，无需因吞吐变化重启循环
  const targetRef = useRef({ rps, tokensPerSec })
  targetRef.current = { rps, tokensPerSec }

  // 每颗粒子的 DOM 引用（固定 MAX 个，靠透明度增减，避免增删节点闪烁）
  const tailRefs = useRef<(SVGPathElement | null)[]>([])
  const haloRefs = useRef<(SVGCircleElement | null)[]>([])
  const headRefs = useRef<(SVGCircleElement | null)[]>([])
  // 每颗粒子沿曲线的相位（0~1，弧长占比），初始均匀铺开成一道流
  const phasesRef = useRef<number[]>(
    Array.from({ length: MAX_PARTICLES }, (_, i) => i / MAX_PARTICLES),
  )
  // 平滑后的密度/速度/亮度（逐帧向目标逼近）
  const easedRef = useRef({ count: 2, speed: 0.14, intensity: 0.12 })

  useEffect(() => {
    if (reduce) return
    if (arc.total <= 0) return
    let raf = 0
    let last = 0
    const idx = MAX_PARTICLES

    const frame = (t: number) => {
      raf = requestAnimationFrame(frame)
      if (last === 0) last = t
      const dt = Math.min((t - last) / 1000, 0.05) // 限幅，防 tab 恢复后跳变
      last = t

      const { rps: r, tokensPerSec: tk } = targetRef.current
      // 目标：密度随 rps、速度随 tokens+rps、亮度综合两者；均给下限保证空闲仍克制可见
      const tgtCount = Math.min(MAX_PARTICLES, Math.max(2, Math.round(2 + r * 7)))
      const tgtSpeed = Math.min(0.6, Math.max(0.14, 0.14 + r * 0.08 + tk / 1400))
      const tgtIntensity = Math.min(1, Math.max(0.12, 0.12 + r * 0.5 + tk / 600))

      // 指数平滑（tau≈0.9s）
      const k = 1 - Math.exp(-dt / 0.9)
      const e = easedRef.current
      e.count += (tgtCount - e.count) * k
      e.speed += (tgtSpeed - e.speed) * k
      e.intensity += (tgtIntensity - e.intensity) * k

      const total = arc.total
      const tailLen = Math.min(0.14, 0.045 + e.speed * 0.14) // 越快尾越长（弧长占比）

      for (let i = 0; i < idx; i++) {
        const head = headRefs.current[i]
        const halo = haloRefs.current[i]
        const tail = tailRefs.current[i]
        if (!head || !halo || !tail) continue

        // 该颗是否在当前密度内（最后一颗按小数做淡入淡出，密度变化不突跳）
        const presence = clamp01(e.count - i)
        if (presence <= 0.001) {
          head.style.opacity = '0'
          halo.style.opacity = '0'
          tail.style.opacity = '0'
          continue
        }

        // 前进相位并回绕
        let p = phasesRef.current[i] + e.speed * dt
        if (p >= 1) p -= Math.floor(p)
        phasesRef.current[i] = p

        // 首尾淡入淡出，回绕处不闪
        const edge = clamp01(p / 0.06) * clamp01((1 - p) / 0.06)
        const baseAlpha = presence * edge * (0.35 + 0.65 * e.intensity)

        const hp = pointAtDist(arc, p * total)
        const rH = 1.5 + e.intensity * 1.6

        head.setAttribute('cx', hp.x.toFixed(1))
        head.setAttribute('cy', hp.y.toFixed(1))
        head.setAttribute('r', rH.toFixed(2))
        head.style.opacity = baseAlpha.toFixed(3)

        halo.setAttribute('cx', hp.x.toFixed(1))
        halo.setAttribute('cy', hp.y.toFixed(1))
        halo.setAttribute('r', (rH * 2.4).toFixed(2))
        halo.style.opacity = (baseAlpha * 0.5).toFixed(3)

        // 拖尾：从头沿曲线往回采样几点连成 path
        const segs = 5
        let d = `M ${hp.x.toFixed(1)},${hp.y.toFixed(1)}`
        for (let s = 1; s <= segs; s++) {
          const f = p - (tailLen * s) / segs
          if (f <= 0) break
          const q = pointAtDist(arc, f * total)
          d += ` L ${q.x.toFixed(1)},${q.y.toFixed(1)}`
        }
        tail.setAttribute('d', d)
        tail.setAttribute('stroke-width', (rH * 1.1).toFixed(2))
        tail.style.opacity = (baseAlpha * 0.45).toFixed(3)
      }
    }

    raf = requestAnimationFrame(frame)
    return () => cancelAnimationFrame(raf)
  }, [arc, reduce])

  if (arc.total <= 0) return null

  // motion-reduce：静态少量淡点（沿曲线均匀铺 5 颗），不做动画
  if (reduce) {
    const dots = Array.from({ length: 5 }, (_, i) => pointAtDist(arc, ((i + 0.5) / 5) * arc.total))
    return (
      <g pointerEvents="none">
        {dots.map((d, i) => (
          <circle key={i} cx={d.x} cy={d.y} r={2} fill="hsl(var(--primary))" opacity={0.4} />
        ))}
      </g>
    )
  }

  return (
    <g pointerEvents="none">
      <defs>
        <filter id={`${gid}-blur`} x="-30%" y="-30%" width="160%" height="160%">
          <feGaussianBlur stdDeviation="2.2" />
        </filter>
      </defs>
      {/* glow 层：拖尾 + 光晕，整体高斯模糊出「辉光拖尾」 */}
      <g filter={`url(#${gid}-blur)`}>
        {Array.from({ length: MAX_PARTICLES }, (_, i) => (
          <g key={i}>
            <path
              ref={(el) => {
                tailRefs.current[i] = el
              }}
              d=""
              fill="none"
              stroke="hsl(var(--primary))"
              strokeLinecap="round"
              strokeLinejoin="round"
              style={{ opacity: 0 }}
            />
            <circle
              ref={(el) => {
                haloRefs.current[i] = el
              }}
              cx={-10}
              cy={-10}
              r={4}
              fill="hsl(var(--primary))"
              style={{ opacity: 0 }}
            />
          </g>
        ))}
      </g>
      {/* core 层：明亮清晰的粒子核心（不模糊） */}
      <g>
        {Array.from({ length: MAX_PARTICLES }, (_, i) => (
          <circle
            key={i}
            ref={(el) => {
              headRefs.current[i] = el
            }}
            cx={-10}
            cy={-10}
            r={1.6}
            fill="hsl(var(--primary))"
            style={{ opacity: 0 }}
          />
        ))}
      </g>
    </g>
  )
}

/**
 * 面积趋势图：真实像素坐标系（ResizeObserver 测宽），Y 轴按数据 min~max 自适应放大，
 * Catmull-Rom 平滑曲线 + 辉光描边 + 渐变面积；非 hover 时淡显峰/谷标记点，让“有数据的趋势”一目了然。
 * hover：高亮圆点跟随鼠标 x 连续移动，y 沿平滑曲线实时插值升降（吸附曲线滑行）；
 * tooltip 数值取最近数据点。失败以底部淡红点含蓄标记。空数据显示占位文案。motion-reduce 兜底。
 */
export function AreaTrendChart({ points, height = 280, showRate = false, granularity = 'hourly', className }: AreaTrendChartProps) {
  const wrapRef = useRef<HTMLDivElement>(null)
  const tipRef = useRef<HTMLDivElement>(null)
  const [width, setWidth] = useState(600)
  // 鼠标在图内的实时 x/y（px，相对容器）；null 表示未 hover。
  // 高亮点/导引线沿曲线用 x 驱动；tooltip 用 x+y 一起驱动，实时跟在鼠标右下角。
  const [mouseX, setMouseX] = useState<number | null>(null)
  const [mouseY, setMouseY] = useState<number | null>(null)
  // tooltip 气泡实测尺寸（px），用于按真实半宽/高做边缘 clamp，避免溢出图容器
  const [tipSize, setTipSize] = useState({ w: 120, h: 56 })
  const fmtLabel = granularity === 'daily' ? fmtDay : fmtHour

  // 实时吞吐：驱动趋势曲线上的流动粒子（读本地内存环，零上游、无封号风险）。
  // 页面隐藏时轮询暂停（见 hook）。取不到时默认 0（粒子降到最稀疏克制态）。
  const { data: throughput } = useUsageThroughput()
  const rps = throughput?.currentRps ?? 0
  const tokensPerSec = throughput?.currentTokensPerSec ?? 0

  useEffect(() => {
    const el = wrapRef.current
    if (!el) return
    const ro = new ResizeObserver((entries) => {
      const w = entries[0]?.contentRect.width
      if (w && w > 0) setWidth(w)
    })
    ro.observe(el)
    return () => ro.disconnect()
  }, [])

  // 实测 tooltip 气泡尺寸：布局提交后同步读取，供边缘 clamp 用真实半宽/高（避免硬编码 60/72 溢出）
  useLayoutEffect(() => {
    const el = tipRef.current
    if (!el) return
    const w = el.offsetWidth
    const h = el.offsetHeight
    if (w > 0 && h > 0 && (Math.abs(w - tipSize.w) > 0.5 || Math.abs(h - tipSize.h) > 0.5)) {
      setTipSize({ w, h })
    }
  })

  const gid = useMemo(() => `area-${Math.random().toString(36).slice(2, 8)}`, [])

  const model = useMemo(() => {
    const pts = points
    const n = pts.length
    const innerH = height - PAD_T - PAD_B
    const reqs = pts.map((p) => p.requests)
    const rawMax = Math.max(1, ...reqs)
    const rawMin = Math.min(...reqs, rawMax)
    const span = Math.max(1, rawMax - rawMin)
    // 收敛式纵向映射：让「数据占图高的比例」贴近真实波动比例，而不是无脑撑满。
    // prop = 波动幅度 / 峰值（真实起伏占比）；据此决定数据带该占多少图高：
    //   小基数大起伏(prop 大)→ 放大观察撑到 ~80%；大基数小波动(prop 小)→ 只占约 15~30%，
    //   避免把微小波动视觉夸大成满屏起伏（此前固定占 ~79%，与真实比例脱节）。
    const prop = span / rawMax
    const visualFrac = Math.max(0.15, Math.min(0.8, 0.15 + prop * 1.5))
    // 数据带该占的图高比例 → 反推总量程；余量偏向下方（约 0.7）保留“浮在基座上”的观感。
    const range = Math.max(1, span / visualFrac)
    const extra = range - span
    const lo = Math.max(0, rawMin - extra * 0.7)
    const hi = lo + range

    const x = (i: number) => PAD_X + (n <= 1 ? 0 : (i / (n - 1)) * (width - PAD_X * 2))
    const y = (v: number) => PAD_T + (1 - (v - lo) / range) * innerH

    const linePts = pts.map((p, i) => ({ x: x(i), y: y(p.requests), p, i }))
    const xy: Pt[] = linePts.map((c) => ({ x: c.x, y: c.y }))
    const segs = buildSegments(xy)
    const linePath = segsToPath(xy, segs)
    const samples = sampleCurve(segs) // 供 hover 连续插值
    const baseY = height - PAD_B
    const areaPath =
      linePts.length > 0
        ? `${linePath} L ${linePts[linePts.length - 1].x.toFixed(1)},${baseY} L ${linePts[0].x.toFixed(1)},${baseY} Z`
        : ''
    const failures = linePts.filter((c) => c.p.failure > 0)

    // 峰/谷标记：非 hover 时淡显，标出全局最高/最低请求点，暗示“这是条有起伏的数据线”
    let peak = linePts[0]
    let valley = linePts[0]
    for (const c of linePts) {
      if (c.p.requests > peak.p.requests) peak = c
      if (c.p.requests < valley.p.requests) valley = c
    }
    const markers = linePts.length > 1 && peak.p.requests !== valley.p.requests ? [peak, valley] : []

    // 成功率副线：固定映射到 0~100%（占绘图区上半带 42%）
    const rateY = (rate: number) => {
      const clamped = Math.max(0, Math.min(1, rate))
      return PAD_T + (1 - clamped) * innerH * 0.42
    }
    const rateXY: Pt[] = pts.map((p, i) => {
      const rate = p.requests > 0 ? p.success / p.requests : 1
      return { x: x(i), y: rateY(rate) }
    })
    const ratePath = segsToPath(rateXY, buildSegments(rateXY))

    return { linePts, linePath, areaPath, failures, baseY, lo, hi, ratePath, samples, markers, rawMax, rawMin }
  }, [points, width, height])

  const hasData = points.some((p) => p.requests > 0)

  // 最近数据点索引（tooltip 取值用）：由鼠标 x 反推
  const nearestIdx = useMemo(() => {
    if (mouseX === null) return null
    const n = model.linePts.length
    if (n === 0) return null
    const usable = width - PAD_X * 2
    const rel = Math.max(0, Math.min(1, (mouseX - PAD_X) / (usable || 1)))
    return Math.round(rel * (n - 1))
  }, [mouseX, model.linePts.length, width])

  if (!hasData) {
    return (
      <div className={className} style={{ height }} role="img" aria-label="暂无请求趋势数据">
        <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
          该区间暂无请求数据
        </div>
      </div>
    )
  }

  const gridYs = [PAD_T, PAD_T + (height - PAD_T - PAD_B) / 2, height - PAD_B]
  const nearest = nearestIdx !== null ? model.linePts[nearestIdx] : null
  // hover 高亮点：x 跟随鼠标，y 沿曲线插值（连续吸附滑行）
  const clampedMouseX = mouseX === null ? null : Math.max(PAD_X, Math.min(width - PAD_X, mouseX))
  const hoverY = clampedMouseX === null ? null : yAtX(model.samples, clampedMouseX)

  const onMove = (e: React.MouseEvent<SVGRectElement>) => {
    const rect = (e.currentTarget as SVGRectElement).getBoundingClientRect()
    setMouseX(e.clientX - rect.left)
    setMouseY(e.clientY - rect.top)
  }
  const onLeave = () => {
    setMouseX(null)
    setMouseY(null)
  }

  return (
    <div ref={wrapRef} className={className} style={{ width: '100%', position: 'relative' }}>
      <svg width={width} height={height} style={{ display: 'block' }}>
        <defs>
          {/* 面积渐变：顶部更实、更亮，向下快速淡出，波峰体积感更强 */}
          <linearGradient id={gid} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="hsl(var(--primary))" stopOpacity="0.42" />
            <stop offset="45%" stopColor="hsl(var(--primary))" stopOpacity="0.16" />
            <stop offset="100%" stopColor="hsl(var(--primary))" stopOpacity="0" />
          </linearGradient>
          {/* 曲线辉光：柔和外发光，让趋势线在深色背景上更突出 */}
          <filter id={`${gid}-glow`} x="-20%" y="-40%" width="140%" height="180%">
            <feGaussianBlur stdDeviation="3" result="blur" />
            <feMerge>
              <feMergeNode in="blur" />
              <feMergeNode in="SourceGraphic" />
            </feMerge>
          </filter>
        </defs>

        {/* 网格线 + 数值标注 */}
        {gridYs.map((gy, i) => (
          <line
            key={i}
            x1={PAD_X}
            y1={gy}
            x2={width - PAD_X}
            y2={gy}
            stroke="hsl(var(--border))"
            strokeWidth="1"
            strokeDasharray="3 4"
            opacity="0.5"
          />
        ))}
        {/* 顶/中/底数值刻度（右对齐贴边，tabular 对齐），提升可读性 */}
        {[model.hi, (model.hi + model.lo) / 2, model.lo].map((v, i) => (
          <text
            key={`t${i}`}
            x={width - PAD_X - 2}
            y={gridYs[i] + (i === 0 ? 10 : i === 2 ? -4 : 3)}
            textAnchor="end"
            className="fill-muted-foreground"
            style={{ fontSize: 10, fontVariantNumeric: 'tabular-nums' }}
            opacity="0.7"
          >
            {compactNum(v)}
          </text>
        ))}

        {/* 面积填充 */}
        <path d={model.areaPath} fill={`url(#${gid})`} />

        {/* 平滑描边线（辉光滤镜 + 加粗），趋势更清晰 */}
        <path
          d={model.linePath}
          fill="none"
          stroke="hsl(var(--primary))"
          strokeWidth="2.5"
          strokeLinecap="round"
          strokeLinejoin="round"
          filter={`url(#${gid}-glow)`}
        />

        {/* 成功率副线（淡绿虚线，0~100% 语义，居上半带） */}
        {showRate && (
          <path
            d={model.ratePath}
            fill="none"
            stroke="hsl(160 84% 45%)"
            strokeWidth="1.5"
            strokeLinecap="round"
            strokeLinejoin="round"
            strokeDasharray="4 3"
            opacity="0.7"
          />
        )}

        {/* 非 hover 峰/谷淡显标记：空心小圈，暗示数据起伏（hover 时隐藏以免干扰） */}
        {mouseX === null &&
          model.markers.map((m, i) => (
            <circle
              key={`m${i}`}
              cx={m.x}
              cy={m.y}
              r="3"
              fill="hsl(var(--background))"
              stroke="hsl(var(--primary))"
              strokeWidth="1.5"
              opacity="0.7"
            />
          ))}

        {/* 沿曲线流动的发光粒子（由实时吞吐驱动密度/速度/亮度）：体现「数据正在传输」。
            置于曲线之上、hover 高亮点之下，不抢交互。 */}
        <FlowParticles samples={model.samples} rps={rps} tokensPerSec={tokensPerSec} />

        {/* 失败：底部含蓄淡红点（仅有失败的柱位） */}
        {model.failures.map((c) => (
          <circle key={c.i} cx={c.x} cy={model.baseY + 6} r="2.5" fill="hsl(0 84% 60%)" opacity="0.55" />
        ))}

        {/* hover：竖向导引线 + 沿曲线滑行的高亮点（x=鼠标, y=曲线插值） */}
        {clampedMouseX !== null && hoverY !== null && (
          <g pointerEvents="none">
            <line
              x1={clampedMouseX}
              y1={PAD_T}
              x2={clampedMouseX}
              y2={model.baseY}
              stroke="hsl(var(--primary))"
              strokeWidth="1"
              opacity="0.4"
            />
            {/* 外层柔光晕圈 */}
            <circle cx={clampedMouseX} cy={hoverY} r="8" fill="hsl(var(--primary))" opacity="0.15" />
            <circle cx={clampedMouseX} cy={hoverY} r="4.5" fill="hsl(var(--primary))" />
            <circle
              cx={clampedMouseX}
              cy={hoverY}
              r="4.5"
              fill="none"
              stroke="hsl(var(--background))"
              strokeWidth="1.5"
            />
          </g>
        )}

        {/* 透明交互层 */}
        <rect x="0" y="0" width={width} height={height} fill="transparent" onMouseMove={onMove} onMouseLeave={onLeave} />
      </svg>

      {/* hover tooltip（HTML 覆盖层）：实时跟在鼠标指针的右下角。
          外层「跟随层」用 transform:translate3d 承载鼠标位移——GPU 合成、只动 transform，
          配合短 ease-out 过渡实现丝滑滑行（transition-[left,top] 走布局回流，不够顺）。
          内层「内容层」独占 animate-rise-in 入场动画，避免和跟随位移的 transform 抢占同一通道。
          left/top 归零，定位完全交给 translate3d。
          定位规则：默认落在鼠标右下角（+OFFSET）；右侧放不下就翻到鼠标左侧、下方放不下就翻到上方，
          最后再按实测尺寸 clamp 兜底，保证气泡始终不出图容器。数值仍取最近数据点。 */}
      {nearest && mouseX !== null && mouseY !== null && (() => {
        const OFFSET = 14
        // 水平：默认鼠标右侧 +OFFSET；右越界则翻到左侧（mouseX - 宽 - OFFSET）
        let tx = mouseX + OFFSET
        if (tx + tipSize.w > width) tx = mouseX - tipSize.w - OFFSET
        tx = Math.max(0, Math.min(width - tipSize.w, tx))
        // 垂直：默认鼠标下方 +OFFSET；下越界则翻到上方（mouseY - 高 - OFFSET）
        let ty = mouseY + OFFSET
        if (ty + tipSize.h > height) ty = mouseY - tipSize.h - OFFSET
        ty = Math.max(0, Math.min(height - tipSize.h, ty))
        return (
          <div
            ref={tipRef}
            className="pointer-events-none absolute left-0 top-0 z-10 will-change-transform transition-transform duration-100 ease-out motion-reduce:transition-none"
            style={{ transform: `translate3d(${tx.toFixed(1)}px, ${ty.toFixed(1)}px, 0)` }}
          >
            <div className="rounded-md border border-border bg-popover px-3 py-2 text-xs shadow-lg animate-rise-in">
              <div className="mb-1 font-medium text-foreground">{fmtLabel(nearest.p.ts_ms)}</div>
              <div className="flex items-center gap-3 tabular-nums text-muted-foreground">
                <span>请求 <span className="font-medium text-foreground">{nearest.p.requests}</span></span>
                <span className="text-emerald-400">成功 {nearest.p.success}</span>
                <span className={nearest.p.failure > 0 ? 'text-red-400' : ''}>失败 {nearest.p.failure}</span>
                {showRate && (
                  <span className="text-emerald-400">
                    成功率 {nearest.p.requests > 0 ? Math.round((nearest.p.success / nearest.p.requests) * 100) : 100}%
                  </span>
                )}
              </div>
            </div>
          </div>
        )
      })()}

      {points.length > 0 && (
        <div className="mt-1 flex justify-between text-[10px] text-muted-foreground">
          <span>{fmtLabel(points[0].ts_ms)}</span>
          <span>{fmtLabel(points[points.length - 1].ts_ms)}</span>
        </div>
      )}
    </div>
  )
}

