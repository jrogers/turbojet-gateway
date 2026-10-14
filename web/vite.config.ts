import react from '@vitejs/plugin-react'
import { defineConfig } from 'vitest/config'

// `npm run dev` sends /api, websockets included, to a gateway: JETBRIDGE_API, or the example
// config's address. An https target with a self-signed certificate needs `secure: false` below.
const api = process.env.JETBRIDGE_API ?? 'http://127.0.0.1:8080'

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/api': { target: api, ws: true },
    },
  },
  test: {
    environment: 'jsdom',
  },
})
