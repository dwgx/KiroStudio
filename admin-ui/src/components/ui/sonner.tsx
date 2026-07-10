// 通知栈已改为自研（src/lib/toaster.tsx，弃用 sonner）。
// 本文件保留为兼容再导出层：App.tsx 仍 `import { Toaster } from '@/components/ui/sonner'`，
// 这里把它转接到自研 Toaster，App 无需改动。视觉/行为/去光污染样式全在 toaster.tsx 内。
export { Toaster } from '@/lib/toaster'
export type { ToasterProps } from '@/lib/toaster'
