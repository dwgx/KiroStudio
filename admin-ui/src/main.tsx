import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import App from './App'
import './i18n' // I18N 初始化(副作用 import,须在任何组件用 useTranslation 前执行)
import './index.css'

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 5000,
      // ⭐ 缓存保留 30 分钟（v5 默认仅 5 分钟）。
      //
      // 为什么必须显式设：各页面的故障降级依赖「error 时仍有 data 可显示」这个前提
      // （`error && !data` 才切错误卡，见 dashboard/settings/overview）。而 v5 的
      // gcTime 默认 5min —— 502 持续超过 5 分钟后缓存被 GC、`data` 变 undefined，
      // 页面仍会退回整页错误卡。也就是说那套降级此前有个**5 分钟有效期上限**，
      // 而实测线上单次部署中断曾达 74 秒、真实故障 p90 71.75s / max 1077s（约 18 分钟），
      // 恰好落在会被 GC 的区间。
      //
      // 代价只是一点内存（这些都是几 KB 的 JSON 快照），换长故障期间面板仍可读。
      gcTime: 30 * 60 * 1000,
      refetchOnWindowFocus: false,
      // 4xx（鉴权/请求错误）不重试——秒失败，不再默认 retry=3 指数退避拖 ~7s（登录卡顿成因）。
      //
      // 5xx 按"是否有希望自愈"分两档：
      // - 网络错误（无 response，如 ERR_NETWORK/超时）与 502/503/504：这些是反代那一跳的
      //   瞬态态（后端部署重启实测中断 74s，期间 Caddy 一路回 502）。只重试 1 次远不够撑过
      //   一次滚动重启，面板会整块转成错误态；放宽到 3 次配合下面的指数退避约覆盖 7s。
      // - 其余 5xx（500 等）是后端确定性业务错误，重试不会变好，维持 1 次即止损。
      retry: (failureCount, error) => {
        const status = (error as { response?: { status?: number } })?.response?.status
        if (status && status >= 400 && status < 500) return false
        const isTransient = status === undefined || status === 502 || status === 503 || status === 504
        return failureCount < (isTransient ? 3 : 1)
      },
      // 显式指数退避（1s/2s/4s…，上限 15s）：默认曲线上限 30s，对 30s 轮询的面板来说
      // 一次退避就跨过下一轮，反而拉长空窗。
      retryDelay: (n) => Math.min(1000 * 2 ** n, 15000),
    },
  },
})

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <App />
    </QueryClientProvider>
  </React.StrictMode>,
)
