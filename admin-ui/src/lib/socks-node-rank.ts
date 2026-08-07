import type { SocksNode } from '@/types/api'

/**
 * 节点自动分配的排序与过滤规则 —— **必须与后端 `resolve_node_plan` 的自动分配一致**。
 *
 * 后端那一支的键是 `(boundCredentials↑, latencyMs↑, id↑)`，并排除
 * `enabled=false` 与 `lastTest.ok=false`；从未测过（`lastTest` 缺失）的**保留**
 * 但排在所有测过的后面。这里逐条对齐：口径不一致会让下拉里的推荐顺序与用户
 * 实际分到的节点对不上，而那种不一致没有任何报错，只能靠逐个点开卡片才发现。
 *
 * 为什么前端也要有一份：「自动分配」按钮要在**提交前**就把选中的节点显示出来
 * （用户得看见自己将要走哪个出口）。让按钮改成"提交时由后端决定"就看不见了。
 */

/** 从未测过的节点排在所有测过的后面（当 `Infinity` 用），而不是被排除。 */
const UNTESTED_LATENCY = Number.POSITIVE_INFINITY

/** 这个节点当前能不能被自动分配（enabled 且不是已知不通）。 */
export function isNodeAssignable(n: SocksNode): boolean {
  if (!n.enabled) return false
  // 只排除**明确失败**的：`lastTest` 缺失（从未测过）仍可用 —— 全新池子里所有节点
  // 都没测过，排除等于池空、全部落直连。
  if (n.lastTest && !n.lastTest.ok) return false
  return true
}

/** 排序键：已绑数↑ → 延迟↑ → id↑（末键只为让顺序稳定）。 */
function rankKey(n: SocksNode): [number, number, number] {
  return [
    n.boundCredentials ?? 0,
    n.lastTest?.ok ? n.lastTest.latencyMs : UNTESTED_LATENCY,
    n.id,
  ]
}

/**
 * 可分配节点按推荐顺序排列（不改入参）。
 *
 * 已绑数是主键而不是延迟：分身的目的就是分散出口，而池里前几个节点常常已被前几批
 * 分身占了 —— 只按延迟排会一直命中同几个出口，"分散"就只发生在本批内部。
 */
export function rankAssignableNodes(nodes: SocksNode[]): SocksNode[] {
  return nodes.filter(isNodeAssignable).sort((a, b) => {
    const ka = rankKey(a)
    const kb = rankKey(b)
    for (let i = 0; i < ka.length; i++) {
      if (ka[i] !== kb[i]) return ka[i] - kb[i]
    }
    return 0
  })
}

/** 「自动分配」按钮：推荐顺序里的第一个（没有可分配节点时 `undefined`）。 */
export function pickBestNode(nodes: SocksNode[]): SocksNode | undefined {
  return rankAssignableNodes(nodes)[0]
}
