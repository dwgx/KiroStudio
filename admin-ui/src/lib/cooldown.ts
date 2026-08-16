/**
 * 冷却原因稳定枚举码 helper（后端 CooldownReason::code()，snake_case 稳定字符串）。
 *
 * 前端判定与 i18n 一律走 code；`cooldownReason`（后端中文）只作展示 fallback。
 * 老后端（无 cooldownCode 字段）时判定函数返回 false，走无害降级分支
 * （只影响颜色/标签，不影响功能）——见 docs/cooldown-reason-i18n-design.md。
 *
 * 本模块保持纯函数（不 import i18n，翻译函数由调用方注入——组件已有 `t`），
 * 以便 `node --test` 直接跑（与 poll-guard.ts 同模式，见 tests/cooldown.test.ts）。
 */

const COOLDOWN_CODES = [
  'rate_limited',
  'suspicious',
  'account_suspended',
  'quota_exhausted',
  'token_refresh_failed',
  'authentication_failed',
  'auth_transient',
  'server_error',
  'model_unavailable',
] as const

export type CooldownCode = (typeof COOLDOWN_CODES)[number]

/** 速率限制（429）类冷却：琥珀色分支（与后端 RateLimitExceeded 对应）。 */
export function isRateLimitCooldown(code: string | undefined): boolean {
  return code === 'rate_limited'
}

/** 可疑活动风控类冷却：最高危分支（与后端 SuspiciousActivity 对应）。 */
export function isSuspiciousCooldown(code: string | undefined): boolean {
  return code === 'suspicious'
}

/** 已知 code → i18n key；未知/缺失 → undefined（调用方 fallback 后端原串）。 */
export function cooldownReasonKey(code: string | undefined): string | undefined {
  if (code && (COOLDOWN_CODES as readonly string[]).includes(code)) {
    return `credentialcard.cooldown.reason.${code}`
  }
  return undefined
}

/**
 * 冷却原因展示文案：已知 code 走 i18n（三语）；未知 code / 老后端（缺失）
 * fallback 到后端原文字符串。
 */
export function cooldownReasonLabel(
  code: string | undefined,
  reason: string | undefined,
  t: (key: string) => string,
): string {
  const key = cooldownReasonKey(code)
  return key ? t(key) : (reason ?? '')
}
