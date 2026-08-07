/**
 * 号池通知的**事件归类**（纯函数，零依赖，可被 `tests/` 直接跑）。
 *
 * 为什么把这几行从 `hooks/use-pool-notifications.ts` 里抽出来：那个 hook 里
 * disabledReason → 通知类别的映射是**分支顺序敏感**的（`QuotaExceeded` 必须排在
 * 兜底 `disabled` 之前），而顺序错了不会报错，只会让通知文案指错排查方向。
 * 归类逻辑留在 hook 里就只能靠人眼看，抽出来才能用测试锁住顺序。
 */

/** 通知类别。每类有独立的 toast 文案与**处置建议**，所以类别 ≠ 原因枚举。 */
export type PoolEventCategory =
  | 'arn'
  | 'quota'
  | 'disabled'
  | 'suspicious'
  | 'regionProbe'
  | 'regionTokenDead'

/**
 * 禁用原因 → 通知类别。
 *
 * 为什么 region 探测的两条要独立成类而不是落 `disabled` 兜底：**处置动作不同**
 * （与后端 `DisabledReason` 上的同款注释一致）——
 * - `RegionProbeFailed`：token 的 region 授权范围与探测候选不交叉 ⇒ 查号的来源区；
 * - `RegionProbeTokenDead`：探测时上游 401，凭据本身已废 ⇒ 换区无用，要重新取 token。
 *
 * 而 `flushBatch` 的 `desc` 是**按类别**给的，同类只能有一句建议 ⇒ 想让两条给出
 * 不同建议就必须分成两类。
 *
 * ⚠️ 分支顺序有意义：`QuotaExceeded` 与两条 region 必须排在兜底之前。
 */
export function classifyDisabledReason(reason?: string): PoolEventCategory {
  // 额度耗尽走 quota：处置是「加号或等下月重置」，与「去凭据管理里看」完全不同。
  if (reason === 'QuotaExceeded') return 'quota'
  if (reason === 'RegionProbeFailed') return 'regionProbe'
  if (reason === 'RegionProbeTokenDead') return 'regionTokenDead'
  return 'disabled'
}
