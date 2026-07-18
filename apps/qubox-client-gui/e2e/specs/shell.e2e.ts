/**
 * Main shell navigation after onboarding is complete.
 * Mocks must be registered then remounted — full page reload wipes __wdio_mocks__.
 */
const DEV_URL = process.env.QUBOX_E2E_DEV_URL ?? "http://127.0.0.1:1420";

async function mockAppCommands() {
  const getOnboarding = await browser.tauri.mock("get_onboarding");
  await getOnboarding.mockResolvedValue({
    completed: true,
    deviceName: "E2E",
    signalingServer: "wss://signal.qubox.app/ws",
  });

  for (const cmd of [
    "get_known_hosts",
    "list_active_sessions",
    "list_pairing_requests",
    "get_settings",
    "discover_lan_hosts",
    "get_host_status",
  ]) {
    const m = await browser.tauri.mock(cmd);
    await m.mockResolvedValue(
      cmd === "get_settings"
        ? {}
        : cmd.includes("list") || cmd.includes("hosts")
          ? []
          : null,
    );
  }
}

async function remountApp() {
  await browser.execute(() => {
    window.__QUBOX_E2E_REMOUNT__?.();
  });
}

describe("Qubox shell", () => {
  beforeEach(async () => {
    await browser.url(DEV_URL);
    await mockAppCommands();
    await remountApp();
    await $("[data-testid='shell-app']").waitForExist({ timeout: 15_000 });
  });

  it("renders sidebar and switches views", async () => {
    await expect($("[data-testid='sidebar']")).toBeDisplayed();
    await expect($("[data-testid='nav-hosts']")).toBeDisplayed();

    await $("[data-testid='nav-settings']").click();
    await expect($("body")).toHaveText(expect.stringMatching(/Signaling|Bitrate|Settings/i));

    await $("[data-testid='nav-host']").click();
    await expect($("body")).toHaveText(expect.stringMatching(/Host/i));
  });
});
