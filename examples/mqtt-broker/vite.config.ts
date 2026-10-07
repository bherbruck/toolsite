import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({
  // Served from a subpath, never the domain root. Without this the build
  // loads and renders blank, because every asset 404s.
  base: '/p/mqtt-broker/',
  plugins: [react(), tailwindcss()],
  // MQTT.js is most of the bundle, and one page needs all of it.
  build: { chunkSizeWarningLimit: 800 },
})
