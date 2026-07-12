import { useLayoutEffect, useRef } from 'react'

/**
 * 轻量 FLIP 动画(First-Last-Invert-Play),零依赖(不引 framer-motion,守项目克制风格)。
 *
 * 用途:号池状态切换排序模式时,让列表项从旧位置平滑滑到新位置(而非瞬间跳)。
 *
 * 用法:
 *   const flipRef = useFlip<HTMLDivElement>([sortKey])   // deps 变化时触发一次 FLIP
 *   <div ref={flipRef}>
 *     {items.map(it => <div key={it.id} data-flip-key={it.id}>…</div>)}
 *   </div>
 *
 * 原理:每次渲染后(useLayoutEffect,DOM 已更新但未绘制),对比容器内每个带 data-flip-key 的子节点
 * 的旧/新位置,先用 transform 把它"拉回"旧位置(视觉不动),再下一帧清 transform + 加 transition,
 * 浏览器就把它从旧位平滑动画到新位。只处理位移(translate),不改布局,GPU 合成、不掉帧。
 */
export function useFlip<T extends HTMLElement>(deps: unknown[]) {
  const containerRef = useRef<T | null>(null)
  const prevRects = useRef<Map<string, DOMRect>>(new Map())

  useLayoutEffect(() => {
    const container = containerRef.current
    if (!container) return
    const nodes = container.querySelectorAll<HTMLElement>('[data-flip-key]')
    const newRects = new Map<string, DOMRect>()

    // 无障碍:用户开启"减少动态"时不做位移动画,只更新基线位置(下次也不会突然滑)。
    const reduceMotion =
      typeof window !== 'undefined' &&
      window.matchMedia?.('(prefers-reduced-motion: reduce)').matches

    nodes.forEach((node) => {
      const key = node.dataset.flipKey
      if (!key) return
      const rect = node.getBoundingClientRect()
      newRects.set(key, rect)
      if (reduceMotion) return
      const prev = prevRects.current.get(key)
      if (!prev) return
      const dx = prev.left - rect.left
      const dy = prev.top - rect.top
      if (dx === 0 && dy === 0) return
      // Invert:先无动画拉回旧位置
      node.style.transition = 'none'
      node.style.transform = `translate(${dx}px, ${dy}px)`
    })

    let raf = 0
    if (!reduceMotion) {
      // Play:下一帧清 transform + 加过渡,浏览器动画到新位;动画结束后**清除行内 transition**,
      // 把控制权还给 CSS class(否则残留 380ms transition 会污染卡片自身的 hover 位移过渡)。
      raf = requestAnimationFrame(() => {
        nodes.forEach((node) => {
          if (node.style.transform && node.style.transform !== 'none') {
            node.style.transition = 'transform 380ms cubic-bezier(0.22, 1, 0.36, 1)'
            node.style.transform = ''
            const cleanup = () => {
              node.style.removeProperty('transition')
              node.style.removeProperty('transform')
              node.removeEventListener('transitionend', cleanup)
            }
            node.addEventListener('transitionend', cleanup)
            // 兜底:transitionend 未触发(被打断/位移为0)时也在 420ms 后清理。
            setTimeout(cleanup, 420)
          }
        })
      })
    }

    prevRects.current = newRects
    return () => {
      if (raf) cancelAnimationFrame(raf)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps)

  return containerRef
}
