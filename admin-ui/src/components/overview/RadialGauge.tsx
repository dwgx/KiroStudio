import { useMemo } from 'react'

export interface RadialGaugeProps {
  /** 0-100 百分比 */
  value: number | null
  /** 直径（px） */
  size?: number
  /** 环宽（px） */
  stroke?: number
  className?: string
}

/** 成功率阈值分级配色：≥90 绿 / <70 红 / 中间黄 */
function gaugeColor(v: number): string {
  if (v >= 90) return 'hsl(160 84% 45%)'
  if (v < 70) return 'hsl(0 84% 60%)'
  return 'hsl(38 92% 55%)'
}

/**
 * 环形进度仪表：双同心 circle，进度弧用 stroke-dasharray + dashoffset 绘制，
 * 从 12 点方向起画（-90° 旋转），dashoffset 带 0.8s 过渡；圆心叠大号百分比。
 * circle 半径固定、容器等比缩放，stroke 宽度不会变形（无需 non-scaling-stroke）。
 */
export function RadialGauge({
  value,
  size = 72,
  stroke = 7,
  className,
}: RadialGaugeProps) {
  const r = (100 - stroke) / 2 // 基于 100x100 viewBox
  const circumference = 2 * Math.PI * r
  const hasValue = value !== null && !Number.isNaN(value)
  const clamped = hasValue ? Math.max(0, Math.min(100, value as number)) : 0
  const color = hasValue ? gaugeColor(clamped) : 'hsl(var(--muted-foreground))'

  const dashoffset = useMemo(
    () => circumference * (1 - clamped / 100),
    [circumference, clamped]
  )

  return (
    <div
      className={className}
      style={{ position: 'relative', width: size, height: size }}
    >
      <svg width={size} height={size} viewBox="0 0 100 100">
        {/* 轨道 */}
        <circle
          cx="50"
          cy="50"
          r={r}
          fill="none"
          stroke="hsl(var(--muted))"
          strokeWidth={stroke}
        />
        {/* 进度弧 */}
        <circle
          cx="50"
          cy="50"
          r={r}
          fill="none"
          stroke={color}
          strokeWidth={stroke}
          strokeLinecap="round"
          strokeDasharray={circumference}
          strokeDashoffset={hasValue ? dashoffset : circumference}
          transform="rotate(-90 50 50)"
          style={{
            transition: 'stroke-dashoffset 0.8s cubic-bezier(0.25,0.46,0.45,0.94), stroke 0.4s ease',
          }}
          className="motion-reduce:transition-none"
        />
      </svg>
      <div
        style={{
          position: 'absolute',
          inset: 0,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
        }}
      >
        <span
          className="font-semibold tabular-nums"
          style={{ fontSize: size * 0.24, color }}
        >
          {hasValue ? `${Math.round(clamped)}%` : '—'}
        </span>
      </div>
    </div>
  )
}
