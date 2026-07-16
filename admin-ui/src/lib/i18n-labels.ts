// 状态/枚举字段的 i18n 映射，集中一处，各组件统一调用。
// 函数每次调用用 i18n 单例取当前语言（非模块 import 时求值）。
import i18n from '@/i18n'

/** 鉴权方式：social=个人 / idc=企业 SSO / api_key=API 密钥。 */
export function authLabel(method: string | null | undefined): string {
  switch (method) {
    case 'social':
      return i18n.t('labels.auth.social')
    case 'idc':
      return i18n.t('labels.auth.idc')
    case 'external_idp':
      return i18n.t('labels.auth.externalIdp')
    case 'api_key':
      return i18n.t('labels.auth.apiKey')
    default:
      return method || i18n.t('labels.common.unknown')
  }
}

/** 鉴权方式的短标签（用于卡片 Badge 等空间受限处）。 */
export function authShortLabel(method: string | null | undefined): string {
  switch (method) {
    case 'social':
      return i18n.t('labels.auth.socialShort')
    case 'idc':
      return i18n.t('labels.auth.idcShort')
    case 'external_idp':
      return i18n.t('labels.auth.externalIdp')
    case 'api_key':
      return i18n.t('labels.auth.apiKey')
    default:
      return method || i18n.t('labels.common.unknown')
  }
}

// 禁用原因：后端下发英文枚举 → i18n key。
const DISABLED_REASON_KEYS: Record<string, string> = {
  Manual: 'labels.disabledReason.manual',
  TooManyFailures: 'labels.disabledReason.tooManyFailures',
  QuotaExceeded: 'labels.disabledReason.quotaExceeded',
  AccountSuspended: 'labels.disabledReason.accountSuspended',
  SuspiciousActivityAuto: 'labels.disabledReason.suspiciousActivityAuto',
  InvalidRefreshToken: 'labels.disabledReason.invalidRefreshToken',
  InvalidConfig: 'labels.disabledReason.invalidConfig',
  TooManyRefreshFailures: 'labels.disabledReason.tooManyRefreshFailures',
  InsufficientBalance: 'labels.disabledReason.insufficientBalance',
  SubscriptionInvalid: 'labels.disabledReason.subscriptionInvalid',
}

/** 禁用原因 -> 当前语言文案；未知值原样返回。 */
export function disabledReasonLabel(reason: string | null | undefined): string {
  if (!reason) return ''
  const key = DISABLED_REASON_KEYS[reason]
  return key ? i18n.t(key) : reason
}

/**
 * 订阅等级：后端下发形如 "KIRO POWER" 的原始标题。
 * 保留原文（品牌名不译），仅在为空时给占位。
 */
export function subscriptionLabel(title: string | null | undefined): string {
  if (!title) return i18n.t('labels.common.unknown')
  return title
}
