import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["self-loop/**/*.test.ts", "src/**/*.test.ts"],
    environment: "node",
    testTimeout: 10_000,
  },
});
