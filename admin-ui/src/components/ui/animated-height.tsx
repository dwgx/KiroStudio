import { useEffect, useRef, useState, type ReactNode } from 'react'

/**
 * AnimatedHeight —— 内容高度变化时平滑过渡容器高度(而非瞬间跳变)。
 *
 * 用途:上号浮窗切换网页/IDC/微软SSO 时各卡片内容高度不同,直接切会"一下子拉长"手感差。
 * 本组件测量内部内容真实高度(ResizeObserver),把外层容器高度做 CSS transition 平滑延展,
 * 配合内容自身的淡入(animate-rise-in),就变成"以当前高度为起点、平滑延展出新内容"的过渡。
 *
 * 实现:外层 height 受控 + overflow-hidden;内层自然高度,ResizeObserver 一变就更新目标高度。
 * 首帧直接用实测高度(不从 0 过渡,避免开屏抖动)。motion-reduce 下不设 transition。
 */
export function AnimatedHeight({
  children,
  className,
  duration = 320,
}: {
  children: ReactNode
  className?: string
  duration?: number
}) {
  const innerRef = useRef<HTMLDivElement | null>(null)
  const [height, setHeight] = useState<number | 'auto'>('auto')
  const firstRef = useRef(true)

  useEffect(() => {
    const el = innerRef.current
    if (!el) return
    const measure = () => {
      const h = el.getBoundingClientRect().height
      setHeight(h)
    }
    measure()
    firstRef.current = false
    const ro = new ResizeObserver(measure)
    ro.observe(el)
    return () => ro.disconnect()
  }, [])

  const reduce =
    typeof window !== 'undefined' && window.matchMedia?.('(prefers-reduced-motion: reduce)').matches

  return (
    <div
      className={className}
      style={{
        height: height === 'auto' ? 'auto' : `${height}px`,
        overflow: 'hidden',
        transition: reduce ? undefined : `height ${duration}ms cubic-bezier(0.16, 1, 0.3, 1)`,
        willChange: 'height',
      }}
    >
      <div ref={innerRef}>{children}</div>
    </div>
  )
}
