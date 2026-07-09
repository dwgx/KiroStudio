import { useEffect, useRef, useState } from 'react'

/**
 * 数值线性滚动动画：值变化时从旧值线性补间到新值（dwgx：前端要"战胜数值增长"，用线性动画表现）。
 *
 * - 纯线性(linear)补间,requestAnimationFrame 驱动,时长按变化幅度自适应(小变化短、大变化略长,有上限)。
 * - format 自定义显示(千分位/紧凑/百分比等),补间的是数字、显示交给 format。
 * - 首次挂载不从 0 滚(直接落定),只有后续"变化"才滚,避免进页面时一堆数字乱跳。
 * - prefers-reduced-motion 下直接落定新值,不做动画。
 * - 非有限数(—/加载态)直接透传,不参与补间。
 */
export function AnimatedNumber({
  value,
  format = (n) => Math.round(n).toLocaleString(),
  durationMs,
  className,
}: {
  value: number
  format?: (n: number) => string
  /** 覆盖自适应时长（毫秒）；不传则按变化幅度自适应 [300, 900]。 */
  durationMs?: number
  className?: string
}) {
  const [display, setDisplay] = useState(value)
  const fromRef = useRef(value)
  const rafRef = useRef<number | null>(null)
  const mountedRef = useRef(false)

  useEffect(() => {
    // 首次挂载：直接落定，不从上一个值滚动
    if (!mountedRef.current) {
      mountedRef.current = true
      fromRef.current = value
      setDisplay(value)
      return
    }

    const from = fromRef.current
    const to = value
    if (from === to) return

    // 无障碍：减少动效时直接落定
    const reduce =
      typeof window !== 'undefined' &&
      window.matchMedia?.('(prefers-reduced-motion: reduce)').matches
    if (reduce) {
      fromRef.current = to
      setDisplay(to)
      return
    }

    // 自适应时长：变化越大滚得略久，钳制在 [300, 900]ms
    const delta = Math.abs(to - from)
    const dur =
      durationMs ?? Math.min(900, Math.max(300, 300 + Math.log10(1 + delta) * 150))

    const start = performance.now()
    if (rafRef.current) cancelAnimationFrame(rafRef.current)

    const tick = (now: number) => {
      const t = Math.min(1, (now - start) / dur)
      // 线性：无缓动，匀速补间
      const cur = from + (to - from) * t
      setDisplay(cur)
      if (t < 1) {
        rafRef.current = requestAnimationFrame(tick)
      } else {
        fromRef.current = to
        rafRef.current = null
      }
    }
    rafRef.current = requestAnimationFrame(tick)

    return () => {
      if (rafRef.current) cancelAnimationFrame(rafRef.current)
    }
  }, [value, durationMs])

  // 非有限数（NaN/Infinity）不参与显示补间
  if (!Number.isFinite(value)) return <span className={className}>{format(value)}</span>
  return <span className={className}>{format(display)}</span>
}
