const API_KEY_STORAGE_KEY = 'adminApiKey'

// adminKey 是管理面凭证,绝不能跨会话残留:localStorage 明文持久化到硬盘,
// 配合全仓无 CSP 的 XSS 等于完整接管管理面,且共用电脑/系统备份都会留下长期泄露窗口。
// 改用 sessionStorage:关闭标签页即清,刷新页面仍在(同一标签内会话不中断)。
// 刻意不做「sessionStorage 为空回退 localStorage」——回退等于保留旧泄露面。
// 旧版本留在 localStorage 的副本在读取时顺手清掉,消灭历史残留。
export const storage = {
  getApiKey: () => {
    localStorage.removeItem(API_KEY_STORAGE_KEY)
    return sessionStorage.getItem(API_KEY_STORAGE_KEY)
  },
  setApiKey: (key: string) => sessionStorage.setItem(API_KEY_STORAGE_KEY, key),
  removeApiKey: () => sessionStorage.removeItem(API_KEY_STORAGE_KEY),
}
