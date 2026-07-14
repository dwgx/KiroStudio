import { Skeleton, SkeletonCard } from '@/components/ui/skeleton'

/**
 * 切页加载骨架屏：替换 app-shell 里那个整页蓝色转圈（#0070f3 border-t）。
 *
 * 按目标标签页的真实布局给出贴合形状的占位——顶部一排统计卡 + 下方内容块，
 * 数据到位后 Suspense 用真实页面淡入替换。相比转圈，用户切页即看到内容轮廓，
 * 体感更快、不焦虑。shimmer 微光 + prefers-reduced-motion 降级由 Skeleton 兜底。
 */

type PageKind = 'overview' | 'credentials' | 'usage' | 'ops' | 'settings'

/** 一排 N 个统计卡骨架（概览/凭据顶部常见三卡布局）。 */
function StatCardRow({ count = 3 }: { count?: number }) {
  return (
    <div className="grid gap-4 md:grid-cols-3">
      {Array.from({ length: count }).map((_, i) => (
        <SkeletonCard key={i} />
      ))}
    </div>
  )
}

/** 大块内容骨架（图表区 / 卡片主体）。 */
function BlockSkeleton({ className }: { className?: string }) {
  return (
    <div className={`card-metal p-5 ${className ?? ''}`}>
      <Skeleton className="h-4 w-32" />
      <Skeleton className="mt-4 h-48 w-full rounded-lg" />
    </div>
  )
}

/** 卡片网格骨架（凭据卡列表）。 */
function CardGridSkeleton({ count = 6 }: { count?: number }) {
  return (
    <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
      {Array.from({ length: count }).map((_, i) => (
        <div key={i} className="card-metal p-5 space-y-3">
          <div className="flex items-center gap-3">
            <Skeleton className="h-9 w-9 shrink-0 rounded-full" />
            <div className="flex-1 space-y-2">
              <Skeleton className="h-3 w-2/5" />
              <Skeleton className="h-3 w-1/4" />
            </div>
          </div>
          <Skeleton className="h-2 w-full rounded-full" />
          <div className="flex gap-2">
            <Skeleton className="h-7 w-16" />
            <Skeleton className="h-7 w-16" />
          </div>
        </div>
      ))}
    </div>
  )
}

/** 竖排卡片堆叠骨架（设置页）。 */
function StackSkeleton({ count = 3 }: { count?: number }) {
  return (
    <div className="space-y-4">
      {Array.from({ length: count }).map((_, i) => (
        <div key={i} className="card-metal p-5 space-y-3">
          <Skeleton className="h-4 w-40" />
          <Skeleton className="h-3 w-full" />
          <Skeleton className="h-3 w-3/4" />
        </div>
      ))}
    </div>
  )
}

export function PageSkeleton({ kind }: { kind: PageKind }) {
  switch (kind) {
    case 'overview':
      return (
        <div className="space-y-4">
          <StatCardRow count={3} />
          <BlockSkeleton />
        </div>
      )
    case 'credentials':
      return (
        <div className="space-y-6">
          <StatCardRow count={3} />
          <CardGridSkeleton count={6} />
        </div>
      )
    case 'usage':
      return (
        <div className="space-y-4">
          <StatCardRow count={4} />
          <BlockSkeleton />
        </div>
      )
    case 'settings':
      return <StackSkeleton count={4} />
    case 'ops':
      return (
        <div className="space-y-4">
          <BlockSkeleton />
          <BlockSkeleton />
        </div>
      )
  }
}
