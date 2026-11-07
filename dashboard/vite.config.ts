/// <reference types="vitest/config" />
import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// `npm run dev` proxies /api to the monitor (local or via the IAP tunnel on 8765).
const target = process.env.ALEPH_API ?? 'http://127.0.0.1:8765'

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    port: 5173,
    strictPort: true,
    proxy: { '/api': { target, changeOrigin: true } },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2023',
    sourcemap: false,
    chunkSizeWarningLimit: 800,
  },
  test: { environment: 'node', include: ['src/**/*.test.ts'] },
})
