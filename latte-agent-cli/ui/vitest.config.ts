import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// Vitest 配置：
//  - React 组件测试 (.test.tsx) 通过 @vitejs/plugin-react 编译 JSX，
//    并在 jsdom 环境下运行（见 environmentMatchGlobs）。
//  - 既有的 vanilla TS 测试 (.test.ts) 仍在默认的 node 环境运行，
//    行为保持不变，不引入回归。
//  - setup 文件为所有测试引入 @testing-library/jest-dom 的匹配器。
export default defineConfig({
  plugins: [react()],
  test: {
    include: [
      "self-loop/**/*.test.ts",
      "src/**/*.test.ts",
      "src/**/*.test.tsx",
    ],
    environment: "node",
    // 仅让 React 组件测试使用 jsdom；其余保持 node。
    environmentMatchGlobs: [["**/*.test.tsx", "jsdom"]],
    setupFiles: ["./src/test-setup.ts"],
    testTimeout: 10_000,
  },
});
