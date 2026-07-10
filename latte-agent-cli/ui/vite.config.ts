import { defineConfig } from 'vite';

// Vite dev server proxy:
//   /api/*  → latte-agent ui backend @ :4567 (set by LATTE_AGENT_UI_PORT env or default)
//   /*      → index.html (SPA fallback)
//
// 在生产模式 (`pnpm build`) 下，整个 ui/ 目录被打成 dist/，
// `latte-agent ui` 命令直接 serve 这个 dist/。

const backend = process.env.VITE_BACKEND ?? 'http://localhost:4567';

export default defineConfig({
  server: {
    host: '0.0.0.0',
    port: Number(process.env.VITE_PORT ?? 5173),
    proxy: {
      '/api': {
        target: backend,
        changeOrigin: true,
      },
      '/health': {
        target: backend,
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    sourcemap: true,
  },
});
