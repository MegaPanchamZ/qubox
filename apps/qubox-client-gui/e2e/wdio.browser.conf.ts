/**
 * Browser-mode e2e: Vite frontend in Chrome with mocked Tauri IPC.
 * No Rust binary / tauri-driver required.
 *
 *   npm run dev          # from apps/qubox-client-gui (port 1420)
 *   npm run test:e2e     # from apps/qubox-client-gui
 */
import { spawn, type ChildProcess } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const appRoot = path.resolve(__dirname, "..");
const DEV_URL = process.env.QUBOX_E2E_DEV_URL ?? "http://127.0.0.1:1420";

let viteProc: ChildProcess | undefined;

async function waitForUrl(url: string, timeoutMs = 60_000): Promise<void> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const res = await fetch(url);
      if (res.ok || res.status === 404) return;
    } catch {
      // not up yet
    }
    await new Promise((r) => setTimeout(r, 300));
  }
  throw new Error(`dev server not reachable at ${url} within ${timeoutMs}ms`);
}

export const config: WebdriverIO.Config = {
  runner: "local",
  specs: ["./specs/**/*.ts"],
  maxInstances: 1,
  capabilities: [
    {
      browserName: "tauri",
      "wdio:tauriServiceOptions": {
        mode: "browser",
        devServerUrl: DEV_URL,
      },
    },
  ],
  logLevel: "warn",
  bail: 0,
  waitforTimeout: 15_000,
  connectionRetryTimeout: 120_000,
  connectionRetryCount: 2,
  framework: "mocha",
  reporters: ["spec"],
  mochaOpts: {
    ui: "bdd",
    timeout: 60_000,
  },
  services: ["@wdio/tauri-service"],

  onPrepare: async () => {
    if (process.env.QUBOX_E2E_SKIP_VITE === "1") {
      await waitForUrl(DEV_URL);
      return;
    }
    viteProc = spawn("npm", ["run", "dev", "--", "--host", "127.0.0.1", "--port", "1420"], {
      cwd: appRoot,
      stdio: "inherit",
      shell: true,
      env: { ...process.env },
    });
    await waitForUrl(DEV_URL);
  },

  onComplete: () => {
    if (viteProc && !viteProc.killed) {
      viteProc.kill("SIGTERM");
    }
  },
};
