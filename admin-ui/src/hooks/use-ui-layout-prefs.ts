import { useSyncExternalStore, useCallback } from 'react'

/**
 * UI 排版自定义偏好(纯前端,localStorage 持久化,跨组件实时同步)。
 *
 * 设计取舍(与 dwgx 对齐):号池状态是**实时数据视图**(每几秒轮询重排),故不做"拖拽固定位置"
 * (会被自动排序/轮询冲掉),改为**排序模式切换** + 禁用号显隐。凭据管理不做手动拖位(多号无意义
 * 且与优先级/分页打架),改为**卡片尺寸档位 → 自适应每行 N 个**。设置页统一配置,两页读此偏好生效。
 */

/** 号池状态排序模式。 */
export type PoolSortMode = 'health' | 'sequence' | 'concurrency' | 'lastUsed'
/** 凭据卡片尺寸档位。 */
export type CardSize = 'compact' | 'standard' | 'large'

export interface UiLayoutPrefs {
  /** 号池排序:health=健康度(默认) / sequence=按 id 顺序 / concurrency=并发在途多优先 / lastUsed=最近调用优先 */
  poolSort: PoolSortMode
  /** 号池状态是否展示已禁用号 */
  poolShowDisabled: boolean
  /** 凭据管理卡片尺寸档位 */
  cardSize: CardSize
}

const STORAGE_KEY = 'uiLayoutPrefs'
const DEFAULTS: UiLayoutPrefs = {
  poolSort: 'health',
  poolShowDisabled: true,
  cardSize: 'standard',
}

// 同一 tab 内 localStorage 写入不触发 storage 事件,用自定义事件广播让所有组件实时同步。
const EVENT = 'ui-layout-prefs-change'

function read(): UiLayoutPrefs {
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    if (!raw) return DEFAULTS
    const parsed = JSON.parse(raw)
    return { ...DEFAULTS, ...parsed }
  } catch {
    return DEFAULTS
  }
}

// useSyncExternalStore 需要稳定的快照引用:缓存上次 JSON 字符串,内容不变则返回同一对象,避免无限重渲染。
let cache: UiLayoutPrefs = read()
let cacheRaw = JSON.stringify(cache)
function getSnapshot(): UiLayoutPrefs {
  const raw = localStorage.getItem(STORAGE_KEY) ?? ''
  if (raw !== cacheRaw) {
    cacheRaw = raw
    cache = read()
  }
  return cache
}

function subscribe(cb: () => void): () => void {
  const handler = () => cb()
  window.addEventListener(EVENT, handler)
  window.addEventListener('storage', handler) // 跨 tab 同步
  return () => {
    window.removeEventListener(EVENT, handler)
    window.removeEventListener('storage', handler)
  }
}

/** 读取 + 修改 UI 排版偏好。任一组件 set 后,所有用此 hook 的组件实时重渲染。 */
export function useUiLayoutPrefs() {
  const prefs = useSyncExternalStore(subscribe, getSnapshot)
  const set = useCallback((patch: Partial<UiLayoutPrefs>) => {
    const next = { ...read(), ...patch }
    localStorage.setItem(STORAGE_KEY, JSON.stringify(next))
    window.dispatchEvent(new Event(EVENT))
  }, [])
  return { prefs, set }
}
