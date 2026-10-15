import react from '@vitejs/plugin-react'
import { defineConfig } from 'vitest/config'

// `npm run dev` sends /api, websockets included, to a gateway: TURBOJET_HUB_API, or the example
// config's address. For an https target with a self-signed certificate, add `secure: false` to the
// proxy entry.
const api = process.env.TURBOJET_HUB_API ?? 'http://127.0.0.1:8080'

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/api': { target: api, ws: true },
    },
  },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/setup.ts'],
  },
})
