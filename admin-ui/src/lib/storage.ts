const API_KEY_STORAGE_KEY = 'adminApiKey'

// adminApiKey 是管理面凭证,绝不能跨会话残留。旧版明文存 localStorage，当时无 CSP
// 的 XSS 等于完整接管管理面。现改用 sessionStorage（关标签即清，刷新仍在），
// 且文档带 CSP 降低 XSS 读到它的面。读取时顺手清掉 localStorage 残留。
// 刻意不做「sessionStorage 为空回退 localStorage」——回退等于保留旧泄露面。
export const storage = {
  getApiKey: () => {
    // 仅当键存在时清一次：历史残留（旧版 localStorage 副本）读取时顺手清掉，
    // 但每次调用都无条件 removeItem 是纯浪费（axios 拦截器每个请求都走这里）。
    if (localStorage.getItem(API_KEY_STORAGE_KEY) !== null) {
      localStorage.removeItem(API_KEY_STORAGE_KEY)
    }
    return sessionStorage.getItem(API_KEY_STORAGE_KEY)
  },
  setApiKey: (key: string) => sessionStorage.setItem(API_KEY_STORAGE_KEY, key),
  removeApiKey: () => sessionStorage.removeItem(API_KEY_STORAGE_KEY),
}
