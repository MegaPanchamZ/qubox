import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // jsdom for @tauri-apps/api/mocks (mockIPC needs window)
    environment: "jsdom",
    include: ["src/**/*.test.ts"],
  },
});
