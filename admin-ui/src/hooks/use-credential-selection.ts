import { useSyncExternalStore } from 'react'

/**
 * 凭据选区共享 store（跨组件读写，模块级状态 + 自定义事件广播）。
 *
 * 为什么要有这东西:选区原先是 `dashboard.tsx` 的局部 `useState` ⇒ 分身视图 / 运维视图
 * 拿不到「当前选中的号」,对选区做任何事都办不到。提出成 store 后任意视图都能读同一份真相。
 *
 * 范式照 `use-ui-layout-prefs.ts`:`useSyncExternalStore` + 自定义事件跨组件同步 +
 * `getSnapshot` 靠缓存引用比对避免无限重渲染。**但有一个关键不同**:
 *
 * 🔴 **刻意不落 localStorage**。选区是**会话态而非偏好**:持久化会让用户下次打开面板时
 * 带着上次的选区去点批量删除。所以只用模块级变量 —— 刷新页面即清空。
 */

/** 选区快照(稳定引用,内容不变则复用同一对象)。 */
export interface CredentialSelectionSnapshot {
  /** 当前选中的凭据 id 集合。只读:改动一律走下面的 action。 */
  ids: ReadonlySet<number>
  /** 区间选(shift 点选)的锚点。无锚点时为 null。 */
  lastAnchorId: number | null
}

// 同一 tab 内模块级变量的写入不会触发任何浏览器事件,用自定义事件广播让所有组件实时同步。
// 刻意不监听 'storage':本 store 不落盘,也不做跨 tab 同步(两个 tab 各自独立选区更符合直觉)。
const EVENT = 'credential-selection-change'

const EMPTY: ReadonlySet<number> = new Set<number>()

// useSyncExternalStore 要求 getSnapshot 在状态未变时返回**同一引用**,否则无限重渲染。
// 这里的做法是:只有 action 真正改变了状态才替换 snapshot 对象,读取永远直接返回它。
let snapshot: CredentialSelectionSnapshot = { ids: EMPTY, lastAnchorId: null }

function getSnapshot(): CredentialSelectionSnapshot {
  return snapshot
}

function subscribe(cb: () => void): () => void {
  const handler = () => cb()
  window.addEventListener(EVENT, handler)
  return () => window.removeEventListener(EVENT, handler)
}

/** 提交新状态并广播。`ids` 内容与 anchor 都没变时**不**替换引用,避免无谓重渲染。 */
function commit(ids: ReadonlySet<number>, lastAnchorId: number | null): void {
  const sameIds = ids === snapshot.ids || (ids.size === snapshot.ids.size && [...ids].every(id => snapshot.ids.has(id)))
  if (sameIds && lastAnchorId === snapshot.lastAnchorId) return
  snapshot = { ids: sameIds ? snapshot.ids : ids, lastAnchorId }
  window.dispatchEvent(new Event(EVENT))
}

/** 加/减选单个 id。选中时把它记为区间选锚点(与 Finder/Polaris 一致:最后一次单点即锚)。 */
export function toggle(id: number): void {
  const next = new Set(snapshot.ids)
  if (next.has(id)) {
    next.delete(id)
    // 取消选中锚点本身则弃锚:留着会让下一次 shift 从一个未选中的位置起算。
    commit(next, snapshot.lastAnchorId === id ? null : snapshot.lastAnchorId)
  } else {
    next.add(id)
    commit(next, id)
  }
}

/** 替换整个选区为单个 id(普通点选语义),并把它记为锚点。 */
export function selectOnly(id: number): void {
  commit(new Set([id]), id)
}

/**
 * 沿 `orderedIds` 给定的顺序,把 [anchorId, toId] 闭区间**并入**选区(不清空既有选区)。
 *
 * ⚠️ **顺序由调用方传入**:不同视图排序不同(凭据管理按 id 分页、号池按健康度…),store 里
 * 不假定任何顺序。`orderedIds` 应是**该视图当前可见且可选**的 id 序列。
 *
 * ⚠️ **`disabled` 号的排除放在调用方**(store 里没有凭据数据,拿不到 disabled 字段)。
 * 调用方传进来的 `orderedIds` 必须**已剔除 disabled 号** —— 与 Polaris 一致,否则 shift
 * 一拖就把禁用号选进「批量启用」里。这是 store 与调用方之间的契约,改调用方时别忘了。
 *
 * anchor 或 to 不在 `orderedIds` 里(例如锚点在上一页、或落在被剔除的 disabled 号上)则
 * 退化为只选 `toId`,并把它记为新锚点。
 */
export function selectRange(anchorId: number, toId: number, orderedIds: readonly number[]): void {
  const from = orderedIds.indexOf(anchorId)
  const to = orderedIds.indexOf(toId)
  if (from < 0 || to < 0) {
    const next = new Set(snapshot.ids)
    next.add(toId)
    commit(next, toId)
    return
  }
  const [lo, hi] = from <= to ? [from, to] : [to, from]
  const next = new Set(snapshot.ids)
  for (let i = lo; i <= hi; i++) next.add(orderedIds[i])
  // 锚点**保持不动**:连续多次 shift 点选都从同一起点重新拉伸,这是各家一致的行为。
  commit(next, anchorId)
}

/** 批量并入(全选/反选等)。锚点不动。 */
export function addMany(ids: readonly number[]): void {
  if (ids.length === 0) return
  const next = new Set(snapshot.ids)
  for (const id of ids) next.add(id)
  commit(next, snapshot.lastAnchorId)
}

/** 批量移出。若锚点被移出则弃锚。 */
export function removeMany(ids: readonly number[]): void {
  if (ids.length === 0 || snapshot.ids.size === 0) return
  const next = new Set(snapshot.ids)
  for (const id of ids) next.delete(id)
  const anchor = snapshot.lastAnchorId
  commit(next, anchor !== null && !next.has(anchor) ? null : anchor)
}

/** 清空选区(同时弃锚)。 */
export function clear(): void {
  commit(EMPTY, null)
}

/**
 * 读取 + 修改凭据选区。任一组件调 action 后,所有用此 hook 的组件实时重渲染。
 *
 * ⚠️ **已知跨页陷阱(本批次不修,下一批处理)**:选区跨分页保留而凭据管理每页只显 12 个 ⇒
 * 第 1 页勾 5 个、翻到第 2 页点「批量删除」,删掉的是**当前看不见的那 5 个**。
 * 迁移到 store 后此行为与迁移前**逐字一致**(原先 `selectedIds` 也不随翻页清空),
 * 刻意不在本批次改动 —— 修法(工具栏显式列出跨页选中项 / 翻页时提示)属下一批。
 */
export function useCredentialSelection() {
  const { ids, lastAnchorId } = useSyncExternalStore(subscribe, getSnapshot)
  // action 全是模块级稳定函数,直接返回即可,无需 useCallback。
  return { ids, lastAnchorId, toggle, selectOnly, selectRange, addMany, removeMany, clear }
}
