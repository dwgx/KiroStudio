import * as React from 'react'
import { cva, type VariantProps } from 'class-variance-authority'
import { AlertTriangle, AlertCircle, Info } from 'lucide-react'
import { cn } from '@/lib/utils'

// 内联告警条：统一的边框/背景/图标（复用 badge 语义色令牌）。
// 替代各处手写的红/黄框 callout，避免风格漂移。variant=danger|warning|info。
const calloutVariants = cva(
  'flex items-start gap-2.5 rounded-md border px-3 py-2.5 text-sm leading-relaxed',
  {
    variants: {
      variant: {
        danger: 'border-red-500/20 bg-red-500/10 text-red-400',
        warning: 'border-amber-500/20 bg-amber-500/10 text-amber-400',
        info: 'border-primary/20 bg-primary/10 text-primary',
      },
    },
    defaultVariants: {
      variant: 'info',
    },
  }
)

const calloutIcons = {
  danger: AlertCircle,
  warning: AlertTriangle,
  info: Info,
} as const

export interface CalloutProps
  extends React.HTMLAttributes<HTMLDivElement>,
    VariantProps<typeof calloutVariants> {}

function Callout({ className, variant, children, ...props }: CalloutProps) {
  const Icon = calloutIcons[variant ?? 'info']
  return (
    <div className={cn(calloutVariants({ variant }), className)} {...props}>
      <Icon className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />
      <div className="min-w-0 flex-1">{children}</div>
    </div>
  )
}

export { Callout, calloutVariants }
