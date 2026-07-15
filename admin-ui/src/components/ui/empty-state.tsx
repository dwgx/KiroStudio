import type { LucideIcon } from 'lucide-react'
import type { ReactNode } from 'react'
import { cn } from '@/lib/utils'

/**
 * 空/错态占位。替换裸的 <p>暂无数据</p> —— 居中图标 + 主副标题 + 可选操作按钮。
 * 暗色友好，尺寸自适应容器。用于运维页各卡片的空态（暂无凭据 / 无匹配日志）与错态（读取失败 + 重试）。
 *
 * - icon：lucide 图标（如 Inbox/ServerCrash/SearchX），可选。
 * - title：主行（必填，一句话）。
 * - description：副行说明（可选）。
 * - action：右下/底部操作节点（如 <Button>重试</Button>），可选。
 * - tone：'muted'（默认，中性空态）| 'destructive'（错态，图标染红）。
 */
export function EmptyState({
  icon: Icon,
  title,
  description,
  action,
  tone = 'muted',
  className,
}: {
  icon?: LucideIcon
  title: ReactNode
  description?: ReactNode
  action?: ReactNode
  tone?: 'muted' | 'destructive'
  className?: string
}) {
  return (
    <div
      className={cn(
        'flex flex-col items-center justify-center gap-3 px-4 py-10 text-center',
        className
      )}
    >
      {Icon && (
        <div
          className={cn(
            'flex h-11 w-11 items-center justify-center rounded-full',
            tone === 'destructive' ? 'bg-red-500/10 text-red-400' : 'bg-secondary text-muted-foreground'
          )}
        >
          <Icon className="h-5 w-5" />
        </div>
      )}
      <div className="space-y-1">
        <p className={cn('text-sm font-medium', tone === 'destructive' ? 'text-red-300' : 'text-foreground')}>
          {title}
        </p>
        {description && <p className="text-xs text-muted-foreground">{description}</p>}
      </div>
      {action && <div className="mt-1">{action}</div>}
    </div>
  )
}
