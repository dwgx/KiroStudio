import * as React from 'react'
import { cn } from '@/lib/utils'

interface ProgressProps extends React.HTMLAttributes<HTMLDivElement> {
  value?: number
  max?: number
  /**
   * 反转颜色语义：默认（false）高百分比=坏=红（用量、占用率）；
   * invert=true 时高百分比=好=绿（健康分、可用率）。仅影响填充色阈值，不改宽度。
   */
  invert?: boolean
}

const Progress = React.forwardRef<HTMLDivElement, ProgressProps>(
  ({ className, value = 0, max = 100, invert = false, ...props }, ref) => {
    const percentage = Math.min(Math.max((value / max) * 100, 0), 100)

    // 默认：>80 红 / >60 黄 / 其余绿（越高越坏）。
    // invert：>=60 绿 / >=30 黄 / 其余红（越高越好，如健康分）。
    const fill = invert
      ? percentage >= 60
        ? 'bg-emerald-500'
        : percentage >= 30
          ? 'bg-amber-400'
          : 'bg-red-500'
      : percentage > 80
        ? 'bg-red-500'
        : percentage > 60
          ? 'bg-yellow-500'
          : 'bg-green-500'

    return (
      <div
        ref={ref}
        className={cn(
          'relative h-4 w-full overflow-hidden rounded-full bg-secondary',
          className
        )}
        {...props}
      >
        <div
          className={cn('h-full transition-all', fill)}
          style={{ width: `${percentage}%` }}
        />
      </div>
    )
  }
)
Progress.displayName = 'Progress'

export { Progress }
