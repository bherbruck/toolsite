import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({
  // Served from a subpath, never the domain root. Without this the build
  // loads and renders blank, because every asset 404s.
  base: '/p/duckdb-report/',
  plugins: [react(), tailwindcss()],
  // DuckDB's wasm and worker are bundled as files of the app, imported with
  // ?url, so the page loads them from its own origin and never a CDN.
  optimizeDeps: { exclude: ['@duckdb/duckdb-wasm'] },
  build: { chunkSizeWarningLimit: 2000 },
})
