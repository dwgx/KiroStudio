import axios, { type AxiosInstance } from 'axios'
import { storage } from '@/lib/storage'
import { shouldClearAdminSession } from './should-clear-admin-session'

// 登录页校验期间抑制"自动 reload 回登录"：登录校验自己 catch 并就地报错，不能被拦截器抢先 reload。
// 状态放在模块级（不是每个实例一份）：抑制是"整个页面此刻"的语义，与用哪个实例无关。
let suppressAuthReload = false
export function setSuppressAuthReload(v: boolean) {
  suppressAuthReload = v
}

/**
 * 建一个 admin API 的 axios 实例：baseURL + 鉴权头 + 会话失效处理，三件套只此一份。
 *
 * 为什么必须是工厂而不是各文件自己 `axios.create`：**拦截器是每个实例各自一份**。
 * 历史（本次审计发现）：`usage.ts` / `ops.ts` 只抄了请求拦截器（x-api-key），
 * 漏了响应拦截器 —— 于是这两个页面上 401/鉴权失败的 403 既不清 sessionStorage 也不回
 * 登录页，轮询一直静默失败，面板看起来"还活着"（实际全是旧数据）。
 * 契约（baseURL / 超时兜底 / 错误形状 `{"error":{"type","message"}}`）也只在这一个文件里改。
 */
export function createAdminApi(): AxiosInstance {
  const api = axios.create({
    baseURL: '/api/admin',
    // 超时兜底：axios 默认 timeout=0 即"永远等"，请求挂在反代那一跳时会一直挂到上游超时
    // （实测 p90 71s / max 1077s），而 React Query 在上一次 fetch 未 settle 前不发下一轮轮询
    // → 整块面板静默冻结。15s 也是后端 `BALANCE_UPSTREAM_TIMEOUT_SECS` 的对齐值（见 balance_cache.rs：
    // 它刻意压在自己这一侧，让面板拿到"上次已知值 + stale"而不是等成前端超时）。
    // 长操作（模型探测/OTA/上号）在各自调用点单独放宽 timeout。
    timeout: 15000,
    headers: {
      'Content-Type': 'application/json',
    },
  })

  // 请求拦截器添加 API Key
  api.interceptors.request.use((config) => {
    const apiKey = storage.getApiKey()
    if (apiKey) {
      config.headers['x-api-key'] = apiKey
    }
    return config
  })

  // 响应拦截器：仅鉴权失败才清 sessionStorage 里的 key 并 reload。
  // 401 = 密钥失效。403 还用于业务拒绝（import_keys 关闭、IP 黑白名单），
  // 只有 error.type === 'authentication_error' 才当鉴权失败；其余交给页面 toast。
  api.interceptors.response.use(
    (res) => res,
    (err) => {
      const status = err?.response?.status as number | undefined
      const rawType = err?.response?.data?.error?.type
      const errorType = typeof rawType === 'string' ? rawType : undefined
      if (shouldClearAdminSession(status, errorType) && !suppressAuthReload) {
        storage.removeApiKey()
        if (typeof window !== 'undefined') {
          window.location.reload()
        }
      }
      return Promise.reject(err)
    },
  )

  return api
}
