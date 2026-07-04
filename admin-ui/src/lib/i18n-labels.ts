// 状态/枚举字段的中文映射，集中一处，各组件统一调用。
// 手写维护，不引入 i18n 库。

/** 鉴权方式：social=个人 / idc=企业 SSO / api_key=API 密钥。 */
export function authLabel(method: string | null | undefined): string {
  switch (method) {
    case 'social':
      return 'Social（个人）'
    case 'idc':
      return 'IdC（企业 SSO）'
    case 'api_key':
      return 'API Key'
    default:
      return method || '未知'
  }
}

/** 鉴权方式的短标签（用于卡片 Badge 等空间受限处）。 */
export function authShortLabel(method: string | null | undefined): string {
  switch (method) {
    case 'social':
      return '个人'
    case 'idc':
      return '企业 SSO'
    case 'api_key':
      return 'API Key'
    default:
      return method || '未知'
  }
}

// 禁用原因：后端下发英文枚举，翻成中文。
const DISABLED_REASON_MAP: Record<string, string> = {
  Manual: '手动禁用',
  TooManyFailures: '失败次数过多',
  QuotaExceeded: '额度耗尽',
  AccountSuspended: '账号封禁',
  InvalidRefreshToken: '刷新令牌失效',
  InvalidConfig: '配置无效',
  TooManyRefreshFailures: '刷新失败过多',
  InsufficientBalance: '余额不足',
}

/** 禁用原因 -> 中文；未知值原样返回。 */
export function disabledReasonLabel(reason: string | null | undefined): string {
  if (!reason) return ''
  return DISABLED_REASON_MAP[reason] ?? reason
}

/**
 * 订阅等级：后端下发形如 "KIRO POWER" 的原始标题。
 * 保留原文（品牌名不译），仅在为空时给占位。
 */
export function subscriptionLabel(title: string | null | undefined): string {
  if (!title) return '未知'
  return title
}
