import { defineConfig, devices } from "@playwright/test";

// Playwright 配置：跑 ui/__tests__/ 下的 e2e。
//
// 默认 baseURL 是 ui 命令起的 :4567 (生产模式)；开发模式（vite）
// 可以 `UI_BASE_URL=http://localhost:5173 pnpm exec playwright test`。
//
// self-loop 会基于这个 e2e 来验证修改 —— 失败 = AI 改坏了。

const BASE_URL = process.env.UI_BASE_URL ?? "http://localhost:4567";

export default defineConfig({
  testDir: "./__tests__",
  timeout: 30_000,
  expect: { timeout: 5_000 },
  retries: 0,
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  workers: 1, // SSE 单连接，避免 broadcast 干扰
  reporter: process.env.CI ? "dot" : "list",
  use: {
    baseURL: BASE_URL,
    headless: true,
    viewport: { width: 1280, height: 800 },
    actionTimeout: 5_000,
    navigationTimeout: 15_000,
    trace: "on-first-retry",
    screenshot: "only-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
  webServer: process.env.UI_NO_WEBSERVER
    ? undefined
    : {
        // 让 playwright 自动起 ui server (production 模式 serve dist/ 是不行的,
        // 因为 dist 可能没 build)。开发场景：用户自己 `latte-agent ui --dev`。
        command: "echo 'set UI_NO_WEBSERVER=1 and start latte-agent ui yourself' && exit 1",
        url: BASE_URL,
        reuseExistingServer: true,
        timeout: 5_000,
      },
});
