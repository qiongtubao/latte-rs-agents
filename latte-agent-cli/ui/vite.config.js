import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
// Vite dev server proxy:
//   /api/*  → latte-agent ui backend @ :4567 (set by LATTE_AGENT_UI_PORT env or default)
//   /*      → index.html (SPA fallback)
//
// 在生产模式 (`pnpm build`) 下，整个 ui/ 目录被打成 dist/，
// `latte-agent ui` 命令直接 serve 这个 dist/。
const backend = process.env.VITE_BACKEND ?? 'http://localhost:4567';
export default defineConfig({
    // React 支持通过 @vitejs/plugin-react 提供（自动 JSX 运行时）。
    // 现有 vanilla TS 模块不受影响，React 能力仅作为增量能力并存。
    plugins: [react()],
    // 相对 base：dist 挂在编辑器 /chat-ui/ 子路径（同源 iframe，阶段 2）
    // 与 server 根路径（latte-agent ui）下都能解析资产 URL。
    base: './',
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
