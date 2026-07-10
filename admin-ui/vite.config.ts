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
      '/api': {
        target: 'http://localhost:8080',
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
})
