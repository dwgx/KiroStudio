import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { CredentialStatusItem } from '@/types/api'
import { healthOf, HEALTH_RGB, statusText } from '@/components/overview/credViz'
import {
  useCanvasLayout,
  resolveLayout,
  clamp,
  type CellLayout,
  MIN_CELL_W,
  MIN_CELL_H,
  MAX_CELL_W,
  MAX_CELL_H,
  GAP,
} from '@/hooks/use-canvas-layout'
import { intersects, normRect, DRAG_THRESHOLD } from '@/lib/marquee-geometry'
import {
  useCredentialSelection,
  selectOnly,
  toggle,
  addMany,
  removeMany,
  clear as clearSelection,
} from '@/hooks/use-credential-selection'

/**
 * 画布视图 —— 与卡片/行视图**并存**的第三档。用户点名要的三件事：
 * **框选多选** / **拖动改位置** / **拖角改大小**。
 *
 * # 位置为什么不会被轮询冲掉
 *
 * 见 `use-canvas-layout.ts` 顶部那段：位置只来自该 store（持久化），
 * 轮询数据只决定**外观**（健康色/在途脉冲）。两者解耦 ⇒ 结构上不可能被重排冲掉。
 * 这是 `use-ui-layout-prefs.ts` 当年否决「拖拽固定位置」那条理由的正解，
 * **不是**违反它。改本文件前先读懂这一段。
 *
 * # 三条交互铁律（踩过才知道的）
 *
 * 1. **必须用 Pointer Events + `setPointerCapture`**，不能用 mouse 事件。指针拖到容器外
 *    再松开时 `mouseup` 会丢，选框/拖动会永久卡住 —— 这是框选最常见的 bug。
 * 2. **4px 拖拽阈值**：小于阈值当点击处理。否则单击会被判成一次「空框选」而清空选区。
 * 3. **命中测试用几何算，不读 DOM**。每个格子的 `(x,y,w,h)` 已在 store 里，
 *    与选框矩形求交即可。用 `getBoundingClientRect()` 逐个测在几百个格子上会掉帧。
 */

/** 交互模式。`idle` 时不挂 move 监听，避免空转。 */
type Mode =
  | { kind: 'idle' }
  | { kind: 'marquee'; x0: number; y0: number; x1: number; y1: number; additive: boolean; subtractive: boolean }
  | { kind: 'drag'; startX: number; startY: number; dx: number; dy: number; base: Map<number, CellLayout> }
  | { kind: 'resize'; id: number; startX: number; startY: number; base: CellLayout }

export interface CredentialCanvasProps {
  credentials: CredentialStatusItem[]
  /** 右键某个号（命中项在选区内时应作用于整个选区，由调用方决定）。 */
  onContextMenu?: (c: CredentialStatusItem, e: React.MouseEvent) => void
  className?: string
}

export function CredentialCanvas({ credentials, onContextMenu, className }: CredentialCanvasProps) {
  const { t } = useTranslation()
  const { layout, setCells, resetAll, pruneLayout } = useCanvasLayout()
  const { ids: selectedIds } = useCredentialSelection()
  const hostRef = useRef<HTMLDivElement>(null)
  const [mode, setMode] = useState<Mode>({ kind: 'idle' })
  const [cols, setCols] = useState(6)

  // 自动排布的下标顺序必须**稳定**：按 id 升序，不受健康度/轮询影响。
  // 用动态顺序会让没摆放过的号在每次轮询后换位。
  const orderedIds = useMemo(
    () => credentials.map((c) => c.id).sort((a, b) => a - b),
    [credentials],
  )

  // 容器宽度决定自动排布列数。ResizeObserver 而非 window.resize：
  // 侧栏折叠/面板分栏变化时容器变了而窗口没变。
  useEffect(() => {
    const el = hostRef.current
    if (!el) return
    const ro = new ResizeObserver((entries) => {
      const w = entries[0]?.contentRect.width ?? 0
      setCols(Math.max(1, Math.floor(w / (MIN_CELL_W + GAP + 60))))
    })
    ro.observe(el)
    return () => ro.disconnect()
  }, [])

  // 号被删掉后清掉它的位置条目（否则表随建/删分身循环单调增长）。
  useEffect(() => {
    if (orderedIds.length > 0) pruneLayout(orderedIds)
  }, [orderedIds, pruneLayout])

  const geom = useMemo(() => resolveLayout(layout, orderedIds, cols), [layout, orderedIds, cols])
  const byId = useMemo(() => new Map(credentials.map((c) => [c.id, c])), [credentials])

  /** 画布逻辑坐标（相对容器左上，含滚动偏移）。 */
  const toLocal = useCallback((e: React.PointerEvent | PointerEvent) => {
    const el = hostRef.current
    if (!el) return { x: 0, y: 0 }
    const r = el.getBoundingClientRect()
    return { x: e.clientX - r.left + el.scrollLeft, y: e.clientY - r.top + el.scrollTop }
  }, [])

  // ── 空白处按下 → 框选 ────────────────────────────────────────────────
  const onHostPointerDown = useCallback(
    (e: React.PointerEvent) => {
      // 只接主键；且忽略落在格子上的（格子自己 stopPropagation）。
      if (e.button !== 0) return
      const p = toLocal(e)
      // 铁律 1：捕获指针，拖到容器外松开也能收到 up。
      e.currentTarget.setPointerCapture(e.pointerId)
      setMode({
        kind: 'marquee',
        x0: p.x,
        y0: p.y,
        x1: p.x,
        y1: p.y,
        additive: e.shiftKey,
        subtractive: e.altKey,
      })
    },
    [toLocal],
  )

  // ── 格子上按下 → 拖动（整个选区一起动）────────────────────────────
  const onCellPointerDown = useCallback(
    (e: React.PointerEvent, id: number) => {
      if (e.button !== 0) return
      e.stopPropagation() // 不要触发容器的框选
      // Cmd/Ctrl 点击 = 加减选，不进入拖动。
      if (e.metaKey || e.ctrlKey) {
        // ⚠️ disabled 号不可选（2026-08-11 审计修复，与行视图同契约）。
        if (credentials.find((c) => c.id === id)?.disabled) return
        toggle(id)
        return
      }
      // 拖一个**不在选区**里的格子：先把选区替换成它（与 Finder 一致）。
      // disabled 号不可拖入选区（同契约）。
      if (credentials.find((c) => c.id === id)?.disabled) return
      const moving = selectedIds.has(id) ? new Set(selectedIds) : new Set([id])
      if (!selectedIds.has(id)) selectOnly(id)
      const base = new Map<number, CellLayout>()
      for (const mid of moving) {
        const g = geom.get(mid)
        if (g) base.set(mid, g)
      }
      e.currentTarget.setPointerCapture(e.pointerId)
      const p = toLocal(e)
      setMode({ kind: 'drag', startX: p.x, startY: p.y, dx: 0, dy: 0, base })
    },
    [credentials, geom, selectedIds, toLocal],
  )

  // ── 右下角手柄按下 → 改大小 ──────────────────────────────────────
  const onResizePointerDown = useCallback(
    (e: React.PointerEvent, id: number) => {
      if (e.button !== 0) return
      e.stopPropagation()
      const g = geom.get(id)
      if (!g) return
      e.currentTarget.setPointerCapture(e.pointerId)
      const p = toLocal(e)
      setMode({ kind: 'resize', id, startX: p.x, startY: p.y, base: g })
    },
    [geom, toLocal],
  )

  // ── 移动：rAF 节流（拖动几百个格子时不掉帧）──────────────────────
  const rafRef = useRef<number | null>(null)
  const pendingRef = useRef<{ x: number; y: number } | null>(null)

  const onPointerMove = useCallback(
    (e: React.PointerEvent) => {
      if (mode.kind === 'idle') return
      pendingRef.current = toLocal(e)
      if (rafRef.current !== null) return
      rafRef.current = requestAnimationFrame(() => {
        rafRef.current = null
        const p = pendingRef.current
        if (!p) return
        setMode((m) => {
          if (m.kind === 'marquee') return { ...m, x1: p.x, y1: p.y }
          if (m.kind === 'drag') return { ...m, dx: p.x - m.startX, dy: p.y - m.startY }
          if (m.kind === 'resize') return m // resize 直接在渲染时按 pending 算
          return m
        })
        // resize 用同一条 rAF，但几何在 commit 时才落盘（拖动过程只改 CSS）。
        if (mode.kind === 'resize') setResizePreview({ w: p.x - mode.startX, h: p.y - mode.startY })
      })
    },
    [credentials, mode, toLocal],
  )

  const [resizePreview, setResizePreview] = useState<{ w: number; h: number } | null>(null)

  // ── 松手：提交 ──────────────────────────────────────────────────
  const onPointerUp = useCallback(
    (e: React.PointerEvent) => {
      if (rafRef.current !== null) {
        cancelAnimationFrame(rafRef.current)
        rafRef.current = null
      }
      try {
        e.currentTarget.releasePointerCapture(e.pointerId)
      } catch {
        // 指针已被别处释放（如格子被卸载）——无害。
      }

      if (mode.kind === 'marquee') {
        const r = normRect(mode.x0, mode.y0, mode.x1, mode.y1)
        // 铁律 2：低于阈值当点击 —— 裸点空白 = 清空选区，带修饰键 = 什么都不做
        // （否则 shift+点空白会意外清掉刚选的一批）。
        if (r.w < DRAG_THRESHOLD && r.h < DRAG_THRESHOLD) {
          if (!mode.additive && !mode.subtractive) clearSelection()
          setMode({ kind: 'idle' })
          return
        }
        // 铁律 3：几何求交，不读 DOM。
        // ⚠️ 剔除 disabled 号（2026-08-11 审计修复，与行视图 marquee 同契约）：
        // store 契约要求调用方不得把禁用号选进选区（use-credential-selection 注释），
        // 否则禁用号会进「批量启用/删除」。单格点选/加减选在 onCellPointerDown 处理。
        const hit: number[] = []
        for (const [id, g] of geom)
          if (intersects(g, r) && !credentials.find((c) => c.id === id)?.disabled) hit.push(id)
        if (mode.subtractive) removeMany(hit)
        else if (mode.additive) addMany(hit)
        else {
          clearSelection()
          addMany(hit)
        }
        setMode({ kind: 'idle' })
        return
      }

      if (mode.kind === 'drag') {
        // 低于阈值 = 点击而非拖动 ⇒ 单选（位置不动）。
        if (Math.abs(mode.dx) < DRAG_THRESHOLD && Math.abs(mode.dy) < DRAG_THRESHOLD) {
          const only = [...mode.base.keys()][0]
          if (only !== undefined) selectOnly(only)
          setMode({ kind: 'idle' })
          return
        }
        const patch: Record<number, CellLayout> = {}
        for (const [id, g] of mode.base) {
          patch[id] = { ...g, x: Math.max(0, g.x + mode.dx), y: Math.max(0, g.y + mode.dy) }
        }
        setCells(patch)
        setMode({ kind: 'idle' })
        return
      }

      if (mode.kind === 'resize') {
        const d = resizePreview
        if (d) {
          setCells({
            [mode.id]: {
              ...mode.base,
              w: clamp(mode.base.w + d.w, MIN_CELL_W, MAX_CELL_W),
              h: clamp(mode.base.h + d.h, MIN_CELL_H, MAX_CELL_H),
            },
          })
        }
        setResizePreview(null)
        setMode({ kind: 'idle' })
      }
    },
    [geom, mode, resizePreview, setCells],
  )

  // Esc 清空选区（画布聚焦时）。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') clearSelection()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  // 画布内容尺寸：取所有格子的右/下边界最大值 + 余量，让容器能滚动到最远的号。
  const extent = useMemo(() => {
    let w = 0
    let h = 0
    for (const g of geom.values()) {
      w = Math.max(w, g.x + g.w)
      h = Math.max(h, g.y + g.h)
    }
    return { w: w + GAP * 4, h: h + GAP * 4 }
  }, [geom])

  const marquee = mode.kind === 'marquee' ? normRect(mode.x0, mode.y0, mode.x1, mode.y1) : null

  return (
    <div className={className}>
      <div className="mb-2 flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-muted-foreground">
        <span>{t('dashboard.canvas.hint')}</span>
        <button
          type="button"
          onClick={resetAll}
          className="rounded border border-border/60 px-2 py-0.5 transition-colors hover:bg-muted/60"
        >
          {t('dashboard.canvas.resetLayout')}
        </button>
        {selectedIds.size > 0 && <span>{t('dashboard.canvas.selected', { n: selectedIds.size })}</span>}
      </div>

      <div
        ref={hostRef}
        onPointerDown={onHostPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerUp}
        // 拖拽期间禁选文本，常驻会让格子里的 id 无法复制。
        className={`relative max-h-[70vh] overflow-auto rounded-lg border border-border/50 bg-card/30 p-3 ${
          mode.kind === 'idle' ? '' : 'select-none'
        }`}
        style={{ touchAction: 'none' }}
      >
        <div className="relative" style={{ width: extent.w, height: extent.h }}>
          {orderedIds.map((id) => {
            const c = byId.get(id)
            const g = geom.get(id)
            if (!c || !g) return null
            const sel = selectedIds.has(id)
            const h = healthOf(c)
            const rgb = HEALTH_RGB[h]
            const dragging = mode.kind === 'drag' && mode.base.has(id)
            const resizing = mode.kind === 'resize' && mode.id === id
            const dx = dragging ? mode.dx : 0
            const dy = dragging ? mode.dy : 0
            const w = resizing && resizePreview ? clamp(g.w + resizePreview.w, MIN_CELL_W, MAX_CELL_W) : g.w
            const hh = resizing && resizePreview ? clamp(g.h + resizePreview.h, MIN_CELL_H, MAX_CELL_H) : g.h
            const inflight = c.inflight ?? 0
            return (
              <div
                key={id}
                onPointerDown={(e) => onCellPointerDown(e, id)}
                onContextMenu={(e) => onContextMenu?.(c, e)}
                className={`absolute overflow-hidden rounded-md border p-2 transition-shadow ${
                  sel ? 'ring-2 ring-primary' : ''
                } ${dragging || resizing ? 'z-20 cursor-grabbing shadow-lg' : 'cursor-grab'}`}
                style={{
                  left: g.x,
                  top: g.y,
                  width: w,
                  height: hh,
                  // 位置用 transform 做拖动预览：不触发 layout，几百个格子也不掉帧。
                  transform: dx || dy ? `translate(${dx}px, ${dy}px)` : undefined,
                  background: `linear-gradient(150deg, rgb(${rgb} / 0.18), rgb(${rgb} / 0.06))`,
                  borderColor: `rgb(${rgb} / ${c.isCurrent ? 0.9 : 0.4})`,
                }}
                title={statusText(c)}
              >
                <div className="flex items-baseline justify-between gap-1">
                  <span className="truncate font-mono text-xs font-medium text-foreground">#{id}</span>
                  {inflight > 0 && (
                    <span className="shrink-0 rounded bg-primary/20 px-1 text-[10px] tabular-nums text-primary">
                      {inflight}
                    </span>
                  )}
                </div>
                <div className="mt-0.5 truncate text-[10px] text-muted-foreground">
                  {c.name || c.email || '—'}
                </div>
                <div className="mt-0.5 text-[10px] tabular-nums text-muted-foreground">
                  {t('dashboard.canvas.rpm')} {c.rpm ?? 0}
                </div>
                {/* 右下角改大小手柄 */}
                <div
                  onPointerDown={(e) => onResizePointerDown(e, id)}
                  className="absolute bottom-0 right-0 h-3 w-3 cursor-nwse-resize opacity-40 transition-opacity hover:opacity-100"
                  style={{
                    background: `linear-gradient(135deg, transparent 50%, rgb(${rgb} / 0.9) 50%)`,
                  }}
                  aria-hidden
                />
              </div>
            )
          })}

          {/* 选框 */}
          {marquee && (marquee.w >= DRAG_THRESHOLD || marquee.h >= DRAG_THRESHOLD) && (
            <div
              className="pointer-events-none absolute z-30 rounded border border-primary bg-primary/10"
              style={{ left: marquee.x, top: marquee.y, width: marquee.w, height: marquee.h }}
            />
          )}
        </div>
      </div>
    </div>
  )
}
