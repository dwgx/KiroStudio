import * as React from 'react'
import { cva, type VariantProps } from 'class-variance-authority'
import { cn } from '@/lib/utils'

const badgeVariants = cva(
  'inline-flex items-center rounded-md border px-2 py-0.5 text-xs font-medium leading-none transition-colors focus:outline-none focus:ring-2 focus:ring-ring focus:ring-offset-2',
  {
    variants: {
      variant: {
        // tinted 风格：低饱和背景 + 同色文字 + 细边框（参考 Linear/GitHub label）
        default:
          'border-primary/20 bg-primary/10 text-primary',
        secondary:
          'border-border bg-secondary text-muted-foreground',
        destructive:
          'border-red-500/20 bg-red-500/10 text-red-400',
        outline:
          'border-border bg-transparent text-muted-foreground',
        success:
          'border-emerald-500/20 bg-emerald-500/10 text-emerald-400',
        warning:
          'border-amber-500/20 bg-amber-500/10 text-amber-400',
      },
    },
    defaultVariants: {
      variant: 'default',
    },
  }
)

export interface BadgeProps
  extends React.HTMLAttributes<HTMLDivElement>,
    VariantProps<typeof badgeVariants> {}

function Badge({ className, variant, ...props }: BadgeProps) {
  return (
    <div className={cn(badgeVariants({ variant }), className)} {...props} />
  )
}

export { Badge, badgeVariants }
