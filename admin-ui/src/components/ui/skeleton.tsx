import { cn } from '@/lib/utils'

/**
 * 通用骨架屏占位块。
 * 暗色友好：低亮度底色 + 一道横向微光扫过（shimmer）。
 * 用于替换整页大转圈——先渲染页面骨架，数据到位再淡出替换。
 *
 * shimmer 动画与 .animate-shimmer 工具类定义在 index.css 末尾。
 * prefers-reduced-motion 下自动降级为静态底色（在 CSS 里兜底）。
 */
function Skeleton({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn('animate-shimmer rounded-md', className)}
      aria-hidden="true"
      {...props}
    />
  )
}

/**
 * 卡片骨架：一行标题条 + 一块大数值条 + 一行辅助说明条，
 * 尺寸贴近 stat-card / card-metal 的常见排版，便于概览页直接顶替。
 */
function SkeletonCard({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div className={cn('card-metal p-5', className)} {...props}>
      <Skeleton className="h-3 w-24" />
      <Skeleton className="mt-4 h-7 w-32" />
      <Skeleton className="mt-3 h-3 w-20" />
    </div>
  )
}

/**
 * 行骨架：左侧圆形头像位 + 两行长短不一的文本条，
 * 适合列表 / 表格行（如用量页记录行）逐行占位。
 */
function SkeletonRow({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div className={cn('flex items-center gap-3', className)} {...props}>
      <Skeleton className="h-9 w-9 shrink-0 rounded-full" />
      <div className="flex-1 space-y-2">
        <Skeleton className="h-3 w-2/5" />
        <Skeleton className="h-3 w-1/4" />
      </div>
    </div>
  )
}

export { Skeleton, SkeletonCard, SkeletonRow }
