import { useEffect, useState } from 'react'

export interface RankItem {
  /** 唯一 key（如凭据 id） */
  id: number | string
  label: string
  value: number
}

export interface RankBarsProps {
  items: RankItem[]
  /** 数值单位后缀，如 "次" */
  unit?: string
  className?: string
}

/**
 * Top 榜单横条：渐变横条 + rank 序号徽标 + 挂载时宽度从 0 动画到目标百分比。
 * 百分比相对榜首归一化；动画用 requestAnimationFrame 触发一次 state 翻转，
 * 避免每次渲染重置为 0。prefers-reduced-motion 下禁用过渡。
 */
export function RankBars({ items, unit = '', className }: RankBarsProps) {
  const [mounted, setMounted] = useState(false)

  useEffect(() => {
    const raf = requestAnimationFrame(() => setMounted(true))
    return () => cancelAnimationFrame(raf)
  }, [])

  if (items.length === 0) {
    return <p className={className}>暂无调用记录</p>
  }

  const max = Math.max(1, ...items.map((it) => it.value))

  return (
    <div className={className}>
      <ol className="space-y-2.5">
        {items.map((it, i) => {
          const pct = Math.round((it.value / max) * 100)
          return (
            <li key={it.id} className="space-y-1">
              <div className="flex items-center justify-between gap-2 text-xs">
                <span className="flex min-w-0 items-center gap-2">
                  <span
                    className={`flex h-4 w-4 shrink-0 items-center justify-center rounded text-[10px] font-semibold tabular-nums ${
                      i === 0
                        ? 'bg-primary/15 text-primary'
                        : 'bg-secondary text-muted-foreground'
                    }`}
                  >
                    {i + 1}
                  </span>
                  <span className="truncate text-muted-foreground">{it.label}</span>
                </span>
                <span className="shrink-0 font-medium tabular-nums text-foreground">
                  {it.value.toLocaleString()}
                  {unit && ` ${unit}`}
                </span>
              </div>
              <div className="h-2 w-full overflow-hidden rounded-full bg-muted">
                <div
                  className="h-full rounded-full transition-[width] duration-700 ease-out-expo motion-reduce:transition-none"
                  style={{
                    width: mounted ? `${pct}%` : '0%',
                    background:
                      'linear-gradient(90deg, hsl(var(--primary)/0.55), hsl(var(--primary)))',
                  }}
                />
              </div>
            </li>
          )
        })}
      </ol>
    </div>
  )
}
