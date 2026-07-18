/**
 * Native-mode e2e: drive the compiled Tauri binary.
 *
 * Requires:
 *   - Built app: cargo build -p qubox-client-gui --release
 *     (binary at workspace target/release/qubox-client-gui)
 *   - Linux: webkit2gtk-driver + xvfb-run recommended
 *   - tauri-plugin-wdio (+ webdriver for embedded) for full mock/execute API
 *
 *   npm run test:e2e:native
 */
import path from "node:path";
import { fileURLToPath } from "node:url";
import os from "node:os";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const workspaceRoot = path.resolve(__dirname, "../../..");

function resolveAppBinary(): string {
  if (process.env.QUBOX_E2E_APP) return process.env.QUBOX_E2E_APP;
  const name =
    os.platform() === "win32" ? "qubox-client-gui.exe" : "qubox-client-gui";
  // Cargo workspace: target/ at repo root
  return path.join(workspaceRoot, "target", "release", name);
}

export const config: WebdriverIO.Config = {
  runner: "local",
  specs: ["./specs/**/*.ts", "./specs/**/*.native.ts"],
  maxInstances: 1,
  capabilities: [
    {
      browserName: "tauri",
      "wdio:tauriServiceOptions": {
        mode: "native",
        // external = tauri-driver + platform WebDriver (Linux/Windows).
        // Use embedded after adding tauri-plugin-wdio-webdriver to the app.
        driverProvider: "external",
        appBinaryPath: resolveAppBinary(),
        autoInstallTauriDriver: true,
      },
    },
  ],
  logLevel: "warn",
  bail: 0,
  waitforTimeout: 20_000,
  connectionRetryTimeout: 180_000,
  connectionRetryCount: 1,
  framework: "mocha",
  reporters: ["spec"],
  mochaOpts: {
    ui: "bdd",
    timeout: 120_000,
  },
  services: ["@wdio/tauri-service"],
};
