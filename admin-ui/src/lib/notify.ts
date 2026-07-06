import { toast } from 'sonner'

// 统一 toast 封装（可选使用）。
// 视觉与图标由 components/ui/sonner.tsx 的 Toaster 统一承担，这里只做
// 「默认时长 + 便捷入口」的薄封装，签名与 sonner 兼容——
// 现有 import { toast } from 'sonner' 的十几处调用点无需改动，照常可用。
//
// 想统一收口的新代码可改用：import { notify } from '@/lib/notify'
//   notify.success('已切换到均衡负载模式')
//   notify.error('切换失败', { description: '网络异常，请重试' })

import type { ExternalToast } from 'sonner'

type Msg = Parameters<typeof toast.success>[0]

// 各态默认展示时长（毫秒）：错误停留更久，成功/信息略短
const DURATION = {
  success: 3200,
  info: 3600,
  warning: 4200,
  error: 5000,
} as const

const withDefault = (d: ExternalToast | undefined, duration: number): ExternalToast => ({
  duration,
  ...d,
})

export const notify = {
  success: (message: Msg, data?: ExternalToast) =>
    toast.success(message, withDefault(data, DURATION.success)),
  error: (message: Msg, data?: ExternalToast) =>
    toast.error(message, withDefault(data, DURATION.error)),
  warning: (message: Msg, data?: ExternalToast) =>
    toast.warning(message, withDefault(data, DURATION.warning)),
  info: (message: Msg, data?: ExternalToast) =>
    toast.info(message, withDefault(data, DURATION.info)),
  // 透传，便于需要 loading/promise/dismiss 的场景直接用
  loading: toast.loading,
  promise: toast.promise,
  dismiss: toast.dismiss,
  message: toast.message,
}

// 兼容再导出：需要原始 toast 的地方也能从这里拿
export { toast }
export default notify
