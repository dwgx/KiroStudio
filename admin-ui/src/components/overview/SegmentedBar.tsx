import { useTranslation } from 'react-i18next'

export interface Segment {
  label: string
  value: number
  /** CSS 颜色（用于分段 + legend 色点） */
  color: string
}

export interface SegmentedBarProps {
  segments: Segment[]
  className?: string
}

/**
 * 单条水平分段条：多色段按占比拼成一条圆角条，下方 legend（色点 + 标签 + 数值 + 百分比）。
 * 段宽用 flex-grow 占比，宽度变化带 CSS 过渡；空数据显示占位。
 */
export function SegmentedBar({ segments, className }: SegmentedBarProps) {
  const { t } = useTranslation()
  const total = segments.reduce((s, seg) => s + seg.value, 0)

  if (total === 0) {
    return (
      <div className={className}>
        <div className="h-2.5 w-full rounded-full bg-muted" />
        <p className="mt-3 text-sm text-muted-foreground">{t('overviewpage.dashboard.summary.noData')}</p>
      </div>
    )
  }

  return (
    <div className={className}>
      <div className="flex h-2.5 w-full overflow-hidden rounded-full bg-muted">
        {segments.map((seg) =>
          seg.value > 0 ? (
            <div
              key={seg.label}
              className="h-full transition-[flex-grow] duration-700 ease-out-expo motion-reduce:transition-none first:rounded-l-full last:rounded-r-full"
              style={{ flexGrow: seg.value, flexBasis: 0, background: seg.color }}
              title={`${seg.label}: ${seg.value}`}
            />
          ) : null
        )}
      </div>
      <div className="mt-3 flex flex-wrap gap-x-4 gap-y-1.5">
        {segments.map((seg) => {
          const pct = Math.round((seg.value / total) * 100)
          return (
            <div key={seg.label} className="flex items-center gap-1.5 text-xs">
              <span
                className="h-2 w-2 shrink-0 rounded-full"
                style={{ background: seg.color }}
              />
              <span className="text-muted-foreground">{seg.label}</span>
              <span className="font-medium tabular-nums text-foreground">
                {seg.value}
              </span>
              <span className="text-muted-foreground">({pct}%)</span>
            </div>
          )
        })}
      </div>
    </div>
  )
}
