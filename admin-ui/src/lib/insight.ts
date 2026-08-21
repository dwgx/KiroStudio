/**
 * 限流 insight 稳定码 helper（后端 `insightCode` + `insightParams`）。
 *
 * 前端展示优先 `t('insight.' + code, params)`；老后端无 code 时 fallback `insightText`。
 * 冷却原因走 `reasonCode` → `cooldownReasonLabel`（与 cooldown.ts 同一套码）。
 */

import { cooldownReasonLabel } from './cooldown.ts'
import type { InsightParams, RateLimitInsight } from '../types/api.ts'

export const INSIGHT_CODES = [
  'clear',
  'disabled',
  'cooldown_rate',
  'cooldown',
  'saturated',
  'saturated_no_spill',
  'near_limit',
  'near_limit_no_spill',
] as const

export type InsightCode = (typeof INSIGHT_CODES)[number]

/** 与 cooldown.ts 同口径：调用方注入 `t`，不在本模块 import i18n。 */
type Translate = (key: string) => string
type TranslateWithOpts = (key: string, options?: Record<string, unknown>) => string

/**
 * 限流 insight 展示文案：有稳定码走 i18n，否则用后端中文 fallback。
 */
export function insightLabel(
  insight: Pick<RateLimitInsight, 'insightText' | 'insightCode' | 'insightParams' | 'cooldown'>,
  t: Translate,
): string {
  const code = insight.insightCode
  if (!code) return insight.insightText
  const raw: InsightParams = insight.insightParams ?? {}
  const reasonCode = raw.reasonCode ?? insight.cooldown?.code
  const params: Record<string, unknown> = { ...raw }
  if (reasonCode) {
    params.reason = cooldownReasonLabel(reasonCode, insight.cooldown?.reason, t)
  }
  const key = 'insight.' + code
  const translated = (t as TranslateWithOpts)(key, params)
  if (!translated || translated === key) return insight.insightText
  return translated
}
