import { defineConfig } from 'vite'
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte'

export default defineConfig({
  root: 'ui',
  plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
  clearScreen: false,
  server: {
    port: Number(process.env.TWORKY_VITE_PORT ?? 1420),
    strictPort: true,
    watch: { ignored: ['**/src-tauri/**', '**/target/**'] },
  },
  build: {
    outDir: '../dist',
    emptyOutDir: true,
    target: 'es2022',
  },
})
