import i18n from '@/i18n'

/**
 * 凭据展示用格式化函数 —— 卡片视图与行视图共用。
 *
 * 原先这五个函数是 `credential-card.tsx` 的模块私有函数。行视图要显示同样的
 * 余额金额/最后调用/代理掩码，若各写一份就会出现"同一个数字两个视图显示不同"
 * 的漂移（本仓在 retries 前端类型上已经踩过两份定义的坑）。故提取到此处，
 * 卡片改为 import，**行为逐字不变**。
 *
 * 这些函数用 `i18n.t`（单例）而非 `useTranslation`：它们在渲染中被调用，
 * 单例取的就是当前语言，切语言时组件重渲染即刷新。
 */

/** 累计花费展示：0 显示 0，小数保留两位，过千用 k 简写，避免长号占满卡片。 */
export function formatCredits(v: number | undefined | null): string {
  const n = typeof v === 'number' && isFinite(v) ? v : 0
  if (n === 0) return '0'
  if (n >= 10000) return `${(n / 1000).toFixed(1)}k`
  return n.toFixed(2)
}

// 每次渲染调用：i18n 单例取当前语言。
export function formatLastUsed(lastUsedAt: string | null): string {
  if (!lastUsedAt) return i18n.t('credentialcard.lastUsed.never')
  const date = new Date(lastUsedAt)
  const now = new Date()
  const diff = now.getTime() - date.getTime()
  if (diff < 0) return i18n.t('credentialcard.lastUsed.justNow')
  const seconds = Math.floor(diff / 1000)
  if (seconds < 60) return i18n.t('credentialcard.lastUsed.secondsAgo', { n: seconds })
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return i18n.t('credentialcard.lastUsed.minutesAgo', { n: minutes })
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return i18n.t('credentialcard.lastUsed.hoursAgo', { n: hours })
  const days = Math.floor(hours / 24)
  return i18n.t('credentialcard.lastUsed.daysAgo', { n: days })
}

// 缓存新鲜度：把 cachedAt（Unix 秒）转成“截至 X 分钟前”，不抹掉数字，只标注时效。
export function formatCachedAt(cachedAt: number): string {
  const diffMs = Date.now() - cachedAt * 1000
  if (diffMs < 0) return i18n.t('credentialcard.lastUsed.justNow')
  const minutes = Math.floor(diffMs / 60000)
  if (minutes < 1) return i18n.t('credentialcard.lastUsed.justNow')
  if (minutes < 60) return i18n.t('credentialcard.lastUsed.minutesAgo', { n: minutes })
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return i18n.t('credentialcard.lastUsed.hoursAgo', { n: hours })
  const days = Math.floor(hours / 24)
  return i18n.t('credentialcard.lastUsed.daysAgo', { n: days })
}

// 代理 URL 脱敏：隐藏 user:pass@ 凭据段，仅保留协议 + 主机:端口。
// socks5://user:pass@1.2.3.4:1080 -> socks5://…@1.2.3.4:1080
export function maskProxyUrl(url: string): string {
  try {
    const u = new URL(url)
    const host = u.host || u.hostname
    if (u.username || u.password) {
      return `${u.protocol}//…@${host}`
    }
    return `${u.protocol}//${host}`
  } catch {
    // 非标准 URL：正则兜底去掉 //cred@ 段
    return url.replace(/\/\/[^@/]*@/, '//…@')
  }
}

// 金额数字格式化：整数时不带小数（6484），有小数时保留一位（87.5）。
export function formatAmount(n: number): string {
  return Number.isInteger(n) ? String(n) : n.toFixed(1)
}
