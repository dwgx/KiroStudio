import { useSyncExternalStore, useCallback } from 'react'

/**
 * 画布视图的**位置与尺寸**持久化（localStorage，跨组件实时同步）。
 *
 * # 为什么这不违反 `use-ui-layout-prefs.ts` 里那条「不做拖拽固定位置」的决定
 *
 * 那条注释否决拖拽的理由是「**会被自动排序/轮询冲掉**」—— 该理由在当时成立：卡片/行视图的
 * 位置是**数据顺序的产物**，每几秒轮询重排一次，手动摆的位置下一次轮询就没了。
 *
 * 画布档绕开它的方式是把**空间位置**与**数据顺序**彻底解耦：
 * - 位置只来自本 store（用户摆的，持久化）——轮询**永不写位置**。
 * - 外观（健康色/脉冲/在途）才来自轮询数据。
 *
 * 于是「轮询冲掉位置」在结构上不可能发生。**卡片/行两档的行为一个字节都没动**，
 * 画布是并存的第三档。改这里前请先读懂上面这段，否则会以为画布违反了既有决定而改回去。
 *
 * # 为什么另开 localStorage 键（不并进 `uiLayoutPrefs`）
 *
 * `uiLayoutPrefs` 是**固定形状的少量标量**（几个枚举 + 布尔），而本 store 是
 * **按凭据 id 索引的映射**，条目数随号池增长（分身上限 16 × N 个号）。两者生命周期也不同：
 * 号被删除后它的位置条目就是垃圾，需要按当前号池做 GC（见 `pruneLayout`），
 * 而排版偏好永不需要 GC。混在一个键里会让「清理无主位置」不得不去动排版偏好。
 */

/** 单个凭据在画布上的几何。单位 px，相对画布左上角。 */
export interface CellLayout {
  x: number
  y: number
  w: number
  h: number
}

/** id → 几何。未出现在表里的号走自动排布（见 `autoPlace`）。 */
export type CanvasLayout = Record<number, CellLayout>

const STORAGE_KEY = 'credentialCanvasLayout'
const EVENT = 'credential-canvas-layout-change'

/** 默认格子尺寸。宽度容得下「#1234」+ 一行状态，高度容得下两行。 */
export const DEFAULT_CELL_W = 132
export const DEFAULT_CELL_H = 76
/** 缩放下限：再小就放不下 id 文本；上限防止一个格子铺满画布。 */
export const MIN_CELL_W = 72
export const MIN_CELL_H = 48
export const MAX_CELL_W = 480
export const MAX_CELL_H = 320
/** 自动排布的列间距/行间距。 */
export const GAP = 12

function read(): CanvasLayout {
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    if (!raw) return {}
    const parsed = JSON.parse(raw)
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return {}
    // 逐条校验：localStorage 可被用户手改，也可能是旧版本写的别的形状。
    // 坏条目**逐个丢弃**而非整表作废 —— 整表作废会让一个脏条目毁掉全部布局。
    const out: CanvasLayout = {}
    for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
      const id = Number(k)
      if (!Number.isFinite(id)) continue
      const c = v as Partial<CellLayout> | null
      if (!c || typeof c !== 'object') continue
      const { x, y, w, h } = c
      if (![x, y, w, h].every((n) => typeof n === 'number' && Number.isFinite(n))) continue
      out[id] = {
        x: Math.max(0, x as number),
        y: Math.max(0, y as number),
        w: clamp(w as number, MIN_CELL_W, MAX_CELL_W),
        h: clamp(h as number, MIN_CELL_H, MAX_CELL_H),
      }
    }
    return out
  } catch {
    return {}
  }
}

export function clamp(n: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, n))
}

// useSyncExternalStore 要求快照引用稳定：内容不变必须返回同一对象，否则每次渲染都判「变了」→ 无限重渲染。
// 与 `use-ui-layout-prefs` 同款：缓存上次原始字符串做比对。
let cache: CanvasLayout = read()
let cacheRaw = localStorage.getItem(STORAGE_KEY) ?? ''

function getSnapshot(): CanvasLayout {
  const raw = localStorage.getItem(STORAGE_KEY) ?? ''
  if (raw !== cacheRaw) {
    cacheRaw = raw
    cache = read()
  }
  return cache
}

function subscribe(cb: () => void): () => void {
  const h = () => cb()
  window.addEventListener(EVENT, h)
  // 跨 tab 同步：localStorage 写入在**其它** tab 才触发 storage 事件。
  window.addEventListener('storage', h)
  return () => {
    window.removeEventListener(EVENT, h)
    window.removeEventListener('storage', h)
  }
}

function write(next: CanvasLayout): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(next))
  } catch {
    // 配额满/隐私模式：位置丢失是可接受的降级（下次进来走自动排布），不该让操作抛错。
  }
  window.dispatchEvent(new CustomEvent(EVENT))
}

/**
 * 未摆放过的号的**确定性**自动位置：按传入顺序的下标铺成网格。
 *
 * 必须是**纯函数**（只看下标与列数），不能用随机或 `Date.now()` ——
 * 否则同一个号每次渲染落在不同位置，画布会在轮询时抖动。
 */
export function autoPlace(index: number, cols: number): CellLayout {
  const c = Math.max(1, cols)
  return {
    x: (index % c) * (DEFAULT_CELL_W + GAP),
    y: Math.floor(index / c) * (DEFAULT_CELL_H + GAP),
    w: DEFAULT_CELL_W,
    h: DEFAULT_CELL_H,
  }
}

/**
 * 解析出「每个号最终用的几何」：存过的用存的，没存过的用自动排布。
 *
 * `orderedIds` 决定自动排布的下标，所以它必须是**稳定顺序**（如按 id 升序），
 * 不能是按健康度排的动态顺序 —— 那会让没摆放过的号在轮询时换位。
 */
export function resolveLayout(
  layout: CanvasLayout,
  orderedIds: readonly number[],
  cols: number,
): Map<number, CellLayout> {
  const out = new Map<number, CellLayout>()
  orderedIds.forEach((id, i) => {
    out.set(id, layout[id] ?? autoPlace(i, cols))
  })
  return out
}

/** 提交一批位置变更（拖动多选时一次落盘，避免逐个写触发 N 次广播）。 */
export function setCells(patch: Record<number, CellLayout>): void {
  const cur = getSnapshot()
  const next: CanvasLayout = { ...cur }
  for (const [k, v] of Object.entries(patch)) {
    const id = Number(k)
    next[id] = {
      x: Math.max(0, v.x),
      y: Math.max(0, v.y),
      w: clamp(v.w, MIN_CELL_W, MAX_CELL_W),
      h: clamp(v.h, MIN_CELL_H, MAX_CELL_H),
    }
  }
  write(next)
}

/** 清掉指定号的自定义位置（回到自动排布）。 */
export function resetCells(ids: readonly number[]): void {
  const cur = getSnapshot()
  const next: CanvasLayout = { ...cur }
  let changed = false
  for (const id of ids) {
    if (id in next) {
      delete next[id]
      changed = true
    }
  }
  if (changed) write(next)
}

/** 全部回到自动排布。 */
export function resetAll(): void {
  if (Object.keys(getSnapshot()).length === 0) return
  write({})
}

/**
 * GC：删掉已不在号池里的位置条目。
 *
 * 不做的话表会随「建分身→删分身」循环单调增长，且旧 id 被复用时会**继承前任的位置**
 * （id 计数器单调递增所以复用罕见，但回收站恢复会让 id 回到池里）。
 * 只在条目确实变少时才写，避免每次挂载都触发一次广播。
 */
export function pruneLayout(liveIds: readonly number[]): void {
  const cur = getSnapshot()
  const live = new Set(liveIds)
  const next: CanvasLayout = {}
  let dropped = 0
  for (const [k, v] of Object.entries(cur)) {
    const id = Number(k)
    if (live.has(id)) next[id] = v
    else dropped += 1
  }
  if (dropped > 0) write(next)
}

/** 读画布布局（响应式）。写操作用模块级函数，不必经 hook。 */
export function useCanvasLayout() {
  const layout = useSyncExternalStore(subscribe, getSnapshot, getSnapshot)
  return {
    layout,
    setCells: useCallback(setCells, []),
    resetCells: useCallback(resetCells, []),
    resetAll: useCallback(resetAll, []),
    pruneLayout: useCallback(pruneLayout, []),
  }
}
