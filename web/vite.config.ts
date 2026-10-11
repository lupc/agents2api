import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// 构建产物输出到 web/dist，由 rust-embed 内嵌进二进制
export default defineConfig({
  plugins: [vue()],
  base: '/admin/',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    port: 5173,
    proxy: {
      // 开发时把 /admin 代理到本服务，无需 CORS
      '/admin': {
        target: 'http://127.0.0.1:8787',
        changeOrigin: true,
      },
    },
  },
})
