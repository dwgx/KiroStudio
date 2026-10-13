/// <reference types="vitest/config" />
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react-swc'
import path from 'path'

export default defineConfig({
  plugins: [react()],
  base: '/admin/',
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
      // 通知系统改为自研（src/lib/toaster.tsx），把 sonner 重定向到它：
      // 现有 `import { toast } from 'sonner'` 的所有调用点零改动、自动解析到 shim。
      sonner: path.resolve(__dirname, './src/lib/toaster.tsx'),
    },
  },
  server: {
    proxy: {
      // 代理到本机后端。端口对齐后端默认 8990（src/model/config.rs default_port，
      // 也是 README / docs 里本地运行的端口）；此前写死 8080 是 hank9999/kiro.rs
      // 基线的遗留值——按文档在 8990 起后端时，pnpm dev 的 /api 请求会全部连不上。
      '/api': {
        target: 'http://localhost:8990',
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  test: {
    environment: 'jsdom',
    // node:test files stay in tests/*.test.ts (tsx-loader). Do not let vitest pick them up.
    include: ['tests/vitest/**/*.test.{ts,tsx}'],
  },
})
