import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({
  // Served from a subpath, never the domain root. Without this the build
  // loads and renders blank, because every asset 404s.
  base: '/p/unbuilt/',
  plugins: [react(), tailwindcss()],
})
