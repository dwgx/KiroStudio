import { useMemo } from 'react'

export interface SparklineProps {
  /** 数值序列（按时间顺序） */
  data: number[]
  /** 线条颜色（CSS color / 变量），默认主色 */
  color?: string
  /** 高度（px），宽度始终撑满容器 */
  height?: number
  className?: string
}

/**
 * 迷你趋势线：SVG viewBox 0 0 100 32 + preserveAspectRatio="none" 水平拉伸铺满，
 * polyline 配 vector-effect="non-scaling-stroke" 防止线宽随非等比缩放变形；
 * 末点高亮圆用绝对定位的 HTML 小圆点渲染（避免在非等比 SVG 内被拉成椭圆）。
 */
export function Sparkline({
  data,
  color = 'hsl(var(--primary))',
  height = 32,
  className,
}: SparklineProps) {
  const geom = useMemo(() => {
    const pts = data.length > 0 ? data : [0]
    const max = Math.max(...pts)
    const min = Math.min(...pts)
    const span = max - min || 1
    // 上下留 3 的内边距，避免顶点/底点贴边被裁切
    const toY = (v: number) => 3 + (1 - (v - min) / span) * (32 - 6)
    const n = pts.length
    const coords = pts.map((v, i) => {
      const x = n === 1 ? 100 : (i / (n - 1)) * 100
      return { x, y: toY(v) }
    })
    const line = coords.map((c) => `${c.x.toFixed(2)},${c.y.toFixed(2)}`).join(' ')
    const area = `0,32 ${line} 100,32`
    const last = coords[coords.length - 1]
    return { line, area, last }
  }, [data])

  const gid = useMemo(() => `spark-${Math.random().toString(36).slice(2, 8)}`, [])
  // 末点相对容器的百分比定位（y 基于 32 单位坐标系）
  const dotTopPct = (geom.last.y / 32) * 100

  return (
    <div className={className} style={{ position: 'relative', width: '100%', height }}>
      <svg
        width="100%"
        height={height}
        viewBox="0 0 100 32"
        preserveAspectRatio="none"
        style={{ display: 'block' }}
      >
        <defs>
          <linearGradient id={gid} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={color} stopOpacity="0.22" />
            <stop offset="100%" stopColor={color} stopOpacity="0" />
          </linearGradient>
        </defs>
        <polygon points={geom.area} fill={`url(#${gid})`} />
        <polyline
          points={geom.line}
          fill="none"
          stroke={color}
          strokeWidth="1.5"
          strokeLinecap="round"
          strokeLinejoin="round"
          vectorEffect="non-scaling-stroke"
        />
      </svg>
      {/* 末点高亮：HTML 圆点，避免 SVG 非等比缩放把 circle 拉成椭圆 */}
      <span
        style={{
          position: 'absolute',
          left: '100%',
          top: `${dotTopPct}%`,
          width: 6,
          height: 6,
          marginLeft: -6,
          marginTop: -3,
          borderRadius: '9999px',
          background: color,
          boxShadow: `0 0 0 2px hsl(var(--card))`,
        }}
      />
    </div>
  )
}
