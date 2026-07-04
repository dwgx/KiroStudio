import { useMemo, useRef, useState, useEffect } from 'react'
import type { SeriesPoint } from '@/types/api'

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

function fmtHour(ts: number): string {
  const d = new Date(ts)
  return `${String(d.getMonth() + 1).padStart(2, '0')}/${String(d.getDate()).padStart(2, '0')} ${String(d.getHours()).padStart(2, '0')}:00`
}

function fmtDay(ts: number): string {
  const d = new Date(ts)
  return `${String(d.getMonth() + 1).padStart(2, '0')}/${String(d.getDate()).padStart(2, '0')}`
}

// Catmull-Rom → 三次贝塞尔平滑路径，让折线更顺滑有质感
function smoothPath(pts: { x: number; y: number }[]): string {
  if (pts.length === 0) return ''
  if (pts.length === 1) return `M ${pts[0].x},${pts[0].y}`
  let d = `M ${pts[0].x.toFixed(1)},${pts[0].y.toFixed(1)}`
  for (let i = 0; i < pts.length - 1; i++) {
    const p0 = pts[i - 1] ?? pts[i]
    const p1 = pts[i]
    const p2 = pts[i + 1]
    const p3 = pts[i + 2] ?? p2
    const cp1x = p1.x + (p2.x - p0.x) / 6
    const cp1y = p1.y + (p2.y - p0.y) / 6
    const cp2x = p2.x - (p3.x - p1.x) / 6
    const cp2y = p2.y - (p3.y - p1.y) / 6
    d += ` C ${cp1x.toFixed(1)},${cp1y.toFixed(1)} ${cp2x.toFixed(1)},${cp2y.toFixed(1)} ${p2.x.toFixed(1)},${p2.y.toFixed(1)}`
  }
  return d
}

/**
 * 面积趋势图：真实像素坐标系（ResizeObserver 测宽，不再 preserveAspectRatio=none 拉伸），
 * Y 轴按数据 min~max 自适应放大，使波峰波谷占据更多垂直空间、波动更明显；
 * Catmull-Rom 平滑曲线 + 渐变面积；失败以底部淡红点含蓄标记；hover 显示该小时明细 tooltip。
 * 空数据显示占位文案。
 */
export function AreaTrendChart({ points, height = 280, showRate = false, granularity = 'hourly', className }: AreaTrendChartProps) {
  const wrapRef = useRef<HTMLDivElement>(null)
  const [width, setWidth] = useState(600)
  const [hoverIdx, setHoverIdx] = useState<number | null>(null)
  // 鼠标在图内的实时 x（px）。tooltip 气泡跟随它平滑滑动，而竖向导引线/高亮点仍吸附最近数据点。
  const [mouseX, setMouseX] = useState<number | null>(null)
  const fmtLabel = granularity === 'daily' ? fmtDay : fmtHour

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

  const gid = useMemo(() => `area-${Math.random().toString(36).slice(2, 8)}`, [])

  const model = useMemo(() => {
    const pts = points
    const n = pts.length
    const innerH = height - PAD_T - PAD_B
    const reqs = pts.map((p) => p.requests)
    const rawMax = Math.max(1, ...reqs)
    const rawMin = Math.min(...reqs, rawMax)
    // 自适应纵向放大：基线取 min 下方留 15% 余量，顶部 max 上方留 12%，
    // 让数据波动铺满绘图区而非挤成一条水平线
    const span = Math.max(1, rawMax - rawMin)
    const lo = Math.max(0, rawMin - span * 0.15)
    const hi = rawMax + span * 0.12
    const range = Math.max(1, hi - lo)

    const x = (i: number) =>
      PAD_X + (n <= 1 ? 0 : (i / (n - 1)) * (width - PAD_X * 2))
    const y = (v: number) => PAD_T + (1 - (v - lo) / range) * innerH

    const linePts = pts.map((p, i) => ({ x: x(i), y: y(p.requests), p, i }))
    const linePath = smoothPath(linePts)
    const baseY = height - PAD_B
    const areaPath =
      linePts.length > 0
        ? `${linePath} L ${linePts[linePts.length - 1].x.toFixed(1)},${baseY} L ${linePts[0].x.toFixed(1)},${baseY} Z`
        : ''
    const failures = linePts.filter((c) => c.p.failure > 0)

    // 成功率副线：固定映射到 0~100%（占绘图区高度 62%~100% 的上半带，避免与主面积图混淆）
    const rateY = (rate: number) => {
      const clamped = Math.max(0, Math.min(1, rate))
      return PAD_T + (1 - clamped) * innerH * 0.42
    }
    const ratePts = pts.map((p, i) => {
      const rate = p.requests > 0 ? p.success / p.requests : 1
      return { x: x(i), y: rateY(rate), rate }
    })
    const ratePath = smoothPath(ratePts)

    return { linePts, linePath, areaPath, failures, baseY, lo, hi, ratePath }
  }, [points, width, height])

  const hasData = points.some((p) => p.requests > 0)

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
  const hovered = hoverIdx !== null ? model.linePts[hoverIdx] : null

  // hover：按鼠标 x 找最近数据点（用于导引线/高亮点），同时记录鼠标实时 x（用于 tooltip 跟随）
  const onMove = (e: React.MouseEvent<SVGRectElement>) => {
    const rect = (e.currentTarget as SVGRectElement).getBoundingClientRect()
    const mx = e.clientX - rect.left
    const n = model.linePts.length
    if (n === 0) return
    const usable = width - PAD_X * 2
    const rel = Math.max(0, Math.min(1, (mx - PAD_X) / (usable || 1)))
    setHoverIdx(Math.round(rel * (n - 1)))
    setMouseX(mx)
  }

  const onLeave = () => {
    setHoverIdx(null)
    setMouseX(null)
  }

  return (
    <div ref={wrapRef} className={className} style={{ width: '100%', position: 'relative' }}>
      <svg width={width} height={height} style={{ display: 'block' }}>
        <defs>
          <linearGradient id={gid} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="hsl(var(--primary))" stopOpacity="0.30" />
            <stop offset="100%" stopColor="hsl(var(--primary))" stopOpacity="0" />
          </linearGradient>
        </defs>

        {/* 网格线 */}
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

        {/* 面积填充 */}
        <path d={model.areaPath} fill={`url(#${gid})`} />

        {/* 平滑描边线 */}
        <path
          d={model.linePath}
          fill="none"
          stroke="hsl(var(--primary))"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
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

        {/* 失败：底部含蓄淡红点（仅有失败的柱位） */}
        {model.failures.map((c) => (
          <circle
            key={c.i}
            cx={c.x}
            cy={model.baseY + 6}
            r="2.5"
            fill="hsl(0 84% 60%)"
            opacity="0.55"
          />
        ))}

        {/* hover 竖向导引线 + 数据点高亮 */}
        {hovered && (
          <g pointerEvents="none">
            <line
              x1={hovered.x}
              y1={PAD_T}
              x2={hovered.x}
              y2={model.baseY}
              stroke="hsl(var(--primary))"
              strokeWidth="1"
              opacity="0.4"
            />
            <circle cx={hovered.x} cy={hovered.y} r="4.5" fill="hsl(var(--primary))" />
            <circle cx={hovered.x} cy={hovered.y} r="4.5" fill="none" stroke="hsl(var(--background))" strokeWidth="1.5" />
          </g>
        )}

        {/* 透明交互层 */}
        <rect
          x="0"
          y="0"
          width={width}
          height={height}
          fill="transparent"
          onMouseMove={onMove}
          onMouseLeave={onLeave}
        />
      </svg>

      {/* hover tooltip（HTML 覆盖层）：气泡跟随鼠标 mouseX 平滑滑动（left transition），
          竖向导引线/高亮点仍吸附最近数据点。tooltip 用 mouseX，回退到数据点 x。 */}
      {hovered && (
        <div
          className="pointer-events-none absolute z-10 -translate-x-1/2 rounded-md border border-border bg-popover px-3 py-2 text-xs shadow-lg animate-rise-in transition-[left] duration-100 ease-out motion-reduce:transition-none"
          style={{
            left: Math.max(60, Math.min(width - 60, mouseX ?? hovered.x)),
            top: Math.max(0, hovered.y - 72),
          }}
        >
          <div className="mb-1 font-medium text-foreground">{fmtLabel(hovered.p.ts_ms)}</div>
          <div className="flex items-center gap-3 tabular-nums text-muted-foreground">
            <span>请求 <span className="font-medium text-foreground">{hovered.p.requests}</span></span>
            <span className="text-emerald-400">成功 {hovered.p.success}</span>
            <span className={hovered.p.failure > 0 ? 'text-red-400' : ''}>失败 {hovered.p.failure}</span>
            {showRate && (
              <span className="text-emerald-400">
                成功率 {hovered.p.requests > 0 ? Math.round((hovered.p.success / hovered.p.requests) * 100) : 100}%
              </span>
            )}
          </div>
        </div>
      )}

      {points.length > 0 && (
        <div className="mt-1 flex justify-between text-[10px] text-muted-foreground">
          <span>{fmtLabel(points[0].ts_ms)}</span>
          <span>{fmtLabel(points[points.length - 1].ts_ms)}</span>
        </div>
      )}
    </div>
  )
}
