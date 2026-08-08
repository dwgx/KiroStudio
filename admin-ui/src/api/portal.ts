import axios from 'axios'
import { storage } from '@/lib/storage'

// 与 credentials/usage/ops 同一套 baseURL 与鉴权拦截：管理面的所有请求都带 adminApiKey。
export const PORTAL_ADMIN_AUTH_EXPIRED_EVENT = 'portal-admin-auth-expired'

const api = axios.create({
  baseURL: '/api/admin/portal',
  withCredentials: true,
  timeout: 15000,
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  // Cookie 会自动随同源请求发送；所有写操作再带不可由普通跨站表单构造的自定义头。
  if ((config.method ?? 'get').toLowerCase() !== 'get') {
    config.headers['x-portal-admin-csrf'] = '1'
  }
  return config
})

api.interceptors.response.use(
  (response) => response,
  (error) => {
    const status = error?.response?.status
    const url = String(error?.config?.url ?? '')
    // 拼车二次会话失效只锁回本页，绝不能清掉主后台 adminApiKey。
    if (status === 401 && !url.startsWith('/auth/')) {
      window.dispatchEvent(new Event(PORTAL_ADMIN_AUTH_EXPIRED_EVENT))
    }
    return Promise.reject(error)
  }
)


/** 拼车管理独立二次认证状态。Cookie 为 HttpOnly，前端只能知道是否已认证。 */
export interface PortalAdminAuthStatus {
  configured: boolean
  authenticated: boolean
  secureTransport: boolean
  expiresInSecs?: number
}

export async function getPortalAdminAuthStatus(): Promise<PortalAdminAuthStatus> {
  const { data } = await api.get<PortalAdminAuthStatus>('/auth/status')
  return data
}

export async function setupPortalAdminPassword(password: string): Promise<{ ok: boolean }> {
  const { data } = await api.post('/auth/setup', { password })
  return data
}

export async function loginPortalAdmin(password: string): Promise<{ ok: boolean }> {
  const { data } = await api.post('/auth/login', { password })
  return data
}

export async function logoutPortalAdmin(): Promise<{ ok: boolean }> {
  const { data } = await api.post('/auth/logout')
  return data
}

export async function changePortalAdminPassword(
  currentPassword: string,
  newPassword: string
): Promise<{ ok: boolean }> {
  const { data } = await api.post('/auth/change-password', { currentPassword, newPassword })
  return data
}

/** 频道总览：开关状态 + 规模。 */
export interface PortalStatus {
  enabled: boolean
  inviteCodeConfigured: boolean
  requireHttps: boolean
  userCount: number
  keyCount: number
}

/**
 * 一个频道用户。
 *
 * **刻意没有 passwordHash 字段**——后端响应里也没有。哈希是离线爆破的原料，
 * 前端连拿到的可能性都不该有（类型上就不给，防止日后有人「顺手」渲染出来）。
 */
export interface PortalUser {
  id: number
  username: string
  disabled: boolean
  createdAtMs: number
  lastLoginMs?: number
  /** 当前积分余额。从未充值过的用户是 0（不是 undefined，后端 COALESCE 过）。 */
  balance: number
  /** 已上车的车队数。判断「这号在用吗」比看最后登录时间更直接。 */
  aboardCount: number
}

/** 一条积分流水。 */
export interface PortalLedgerEntry {
  id: number
  atMs: number
  /** 正 = 进账（充值/退款），负 = 出账（上车）。 */
  delta: number
  /** 写入时的余额快照，对账时不必重放全部历史。 */
  balanceAfter: number
  /** `topup` / `unlock` / `refund` / `admin_adjust`。 */
  kind: string
  credentialId?: number
  note?: string
}

/** 某个用户的钱包详情。 */
export interface PortalWallet {
  balance: number
  topup: number
  /** 净支出：退款会让它变小，所以不是单调递增的。 */
  spent: number
  ledger: PortalLedgerEntry[]
}

/**
 * 当前车费规则。
 *
 * `priceTable` 是后端用**真实计价函数**算出来的 1..max 单价序列。
 * 前端直接显示它、不自己复算：那个公式（两段式 + ceil + min 钳制）一旦有两份实现，
 * 面板显示的价和实际扣的分就可能不一致，而用户只会相信自己看到的那个。
 */
export interface PortalPricing {
  enabled: boolean
  baseCount: number
  basePrice: number
  totalPrice: number
  minPrice: number
  maxBoarders: number
  priceTable: number[]
}

/** 一条审计。登录成败、明文外显、管理员操作都在这里。 */
export interface PortalAuditEntry {
  id: number
  atMs: number
  username?: string
  action: string
  clientIp?: string
  detail?: string
}

export async function getPortalStatus(): Promise<PortalStatus> {
  const { data } = await api.get<PortalStatus>('/status')
  return data
}

export async function listPortalUsers(): Promise<PortalUser[]> {
  const { data } = await api.get<PortalUser[]>('/users')
  return data
}

export async function createPortalUser(
  username: string,
  password: string
): Promise<{ id: number; ok: boolean }> {
  const { data } = await api.post('/users', { username, password })
  return data
}

export async function resetPortalPassword(
  id: number,
  password: string
): Promise<{ ok: boolean }> {
  const { data } = await api.post(`/users/${id}/password`, { password })
  return data
}

export async function setPortalUserDisabled(
  id: number,
  disabled: boolean
): Promise<{ ok: boolean }> {
  const { data } = await api.post(`/users/${id}/disabled`, { disabled })
  return data
}

export async function deletePortalUser(id: number): Promise<{ ok: boolean }> {
  const { data } = await api.delete(`/users/${id}`)
  return data
}

export async function getPortalAudit(): Promise<PortalAuditEntry[]> {
  const { data } = await api.get<PortalAuditEntry[]>('/audit')
  return data
}

/**
 * 手动充值或扣减。`amount` 为正是加分，为负是扣分。
 *
 * 这是积分进入系统的唯一入口——没有自动发放、没有签到，所以每一分都对应
 * 一次管理员的显式操作，对账时不存在「这分哪来的」这种问题。
 *
 * 扣减超过余额时后端返回 400（不会静默截断到 0）：想扣 50 却只扣掉 30
 * 而界面显示「成功」，比直接失败更糟。
 */
export async function topupPortalUser(
  id: number,
  amount: number,
  note?: string
): Promise<{ ok: boolean; balance: number }> {
  const { data } = await api.post(`/users/${id}/topup`, { amount, note })
  return data
}

export async function getPortalUserWallet(id: number): Promise<PortalWallet> {
  const { data } = await api.get<PortalWallet>(`/users/${id}/wallet`)
  return data
}

export async function getPortalPricing(): Promise<PortalPricing> {
  const { data } = await api.get<PortalPricing>('/pricing')
  return data
}
