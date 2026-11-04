import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  build: {
    outDir: 'dist',
  },
  server: {
    port: 5173,
    // Proxy /api to the dust-serve backend during development.
    proxy: {
      '/api': 'http://localhost:8080',
    },
  },
})
