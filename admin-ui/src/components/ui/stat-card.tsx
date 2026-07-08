import * as React from 'react'
import type { LucideIcon } from 'lucide-react'
import { cn } from '@/lib/utils'

/** 语义强调色，驱动图标底色与数值色调 */
export type StatAccent = 'neutral' | 'primary' | 'success' | 'warning' | 'destructive'

const accentStyles: Record<
  StatAccent,
  { icon: string; value: string; dot: string }
> = {
  neutral: {
    icon: 'bg-secondary text-muted-foreground',
    value: 'text-foreground',
    dot: 'bg-muted-foreground',
  },
  primary: {
    icon: 'bg-primary/10 text-primary',
    value: 'text-foreground',
    dot: 'bg-primary',
  },
  success: {
    icon: 'bg-emerald-500/10 text-emerald-400',
    value: 'text-foreground',
    dot: 'bg-emerald-400',
  },
  warning: {
    icon: 'bg-amber-500/10 text-amber-400',
    value: 'text-foreground',
    dot: 'bg-amber-400',
  },
  destructive: {
    icon: 'bg-red-500/10 text-red-400',
    value: 'text-foreground',
    dot: 'bg-red-400',
  },
}

export interface StatCardProps {
  /** 顶部小标签 */
  label: string
  /** 主数值（大号） */
  value: React.ReactNode
  /** 数值下方的辅助说明，可为纯文本或自定义节点 */
  hint?: React.ReactNode
  /** 右上角点缀图标 */
  icon?: LucideIcon
  /** 语义强调色 */
  accent?: StatAccent
  className?: string
}

/**
 * 通用 KPI 统计卡：大数字 + 小标签 + 语义色 + 图标点缀。
 * 概览页与仪表盘复用同一套视觉。
 */
export function StatCard({
  label,
  value,
  hint,
  icon: Icon,
  accent = 'neutral',
  className,
}: StatCardProps) {
  const styles = accentStyles[accent]
  return (
    // 金属厚重卡：复用 .card-metal-press（金属渐变背景 + 厚重阴影，hover 时轻微下压、阴影收敛，不缩放不凹陷）
    // motion-reduce 下由 .card-metal-press 的媒体查询自动取消位移
    <div className={cn('card-metal-press p-5', className)}>
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 space-y-2">
          <p className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
            {label}
          </p>
          <div className={cn('truncate text-3xl font-bold leading-none tracking-tight tabular-nums', styles.value)}>
            {value}
          </div>
        </div>
        {Icon && (
          <div
            className={cn(
              'flex h-9 w-9 shrink-0 items-center justify-center rounded-lg',
              styles.icon
            )}
          >
            <Icon className="h-[18px] w-[18px]" />
          </div>
        )}
      </div>
      {hint && (
        <div className="mt-3 flex items-center gap-1.5 text-xs text-muted-foreground">
          {typeof hint === 'string' ? (
            <>
              <span className={cn('h-1.5 w-1.5 rounded-full', styles.dot)} />
              <span className="truncate">{hint}</span>
            </>
          ) : (
            hint
          )}
        </div>
      )}
    </div>
  )
}
