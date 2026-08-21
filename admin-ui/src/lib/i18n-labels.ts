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
  // 后端早已下发但此前漏映射 → 面板显示的是裸英文枚举名。
  RequestLimitReached: 'labels.disabledReason.requestLimitReached',
  // 代挂号（第三方中转站）专用。与 TooManyFailures 分开是因为排查方向完全不同：
  // 那条查 Kiro 号是否被风控，这两条查中转站的 key/余额/地址，或站点是否持续过载。
  PassthroughFailed: 'labels.disabledReason.passthroughFailed',
  PassthroughOverloaded: 'labels.disabledReason.passthroughOverloaded',
  // 上号时 region 自动探测的两种失败。与 TooManyFailures 分开是因为排查方向不同：
  // 这两条查「token 的 region 授权范围」或「token 本身是否已废」，而且都不可自愈
  // （自愈白名单不含它们），人工确认后需手动启用。
  RegionProbeFailed: 'labels.disabledReason.regionProbeFailed',
  RegionProbeTokenDead: 'labels.disabledReason.regionProbeTokenDead',
  // 反序列化兜底：读到本版本不认识的原因（如回滚后读新版写的文件）时后端下发 Unknown。
  Unknown: 'labels.disabledReason.unknown',
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

// 存储分区：后端下发中文 label，按稳定 key 映射本地化名；未知 key 回退后端值。
const STORAGE_PART_KEYS: Record<string, string> = {
  traces: 'opspage.storage.part.traces',
  usage_jsonl: 'opspage.storage.part.usage_jsonl',
  trash: 'opspage.storage.part.trash',
  bg_cache: 'opspage.storage.part.bg_cache',
  rss: 'opspage.storage.part.rss',
}

/** 存储分区名：优先本地化 key，未知 key 原样返回后端 label。 */
export function storagePartitionLabel(key: string | null | undefined, fallback: string): string {
  const i18nKey = key ? STORAGE_PART_KEYS[key] : undefined
  return i18nKey ? i18n.t(i18nKey) : fallback
}
