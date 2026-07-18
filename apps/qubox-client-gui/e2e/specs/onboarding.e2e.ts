/**
 * First-run / onboarding flow (browser mode with mocked IPC).
 * Mocks must be registered then remounted — full page reload wipes __wdio_mocks__.
 */
const DEV_URL = process.env.QUBOX_E2E_DEV_URL ?? "http://127.0.0.1:1420";

async function remountApp() {
  await browser.execute(() => {
    window.__QUBOX_E2E_REMOUNT__?.();
  });
}

describe("Qubox onboarding", () => {
  beforeEach(async () => {
    await browser.url(DEV_URL);

    const getOnboarding = await browser.tauri.mock("get_onboarding");
    await getOnboarding.mockRejectedValue(new Error("daemon offline"));
    await remountApp();

    await $("[data-testid='first-run'], [data-testid='shell-loading']").waitForExist({
      timeout: 15_000,
    });
    await $("[data-testid='first-run']").waitForExist({ timeout: 15_000 });
  });

  it("shows cloud vs self-host mode chooser", async () => {
    await expect($("[data-testid='first-run-title']")).toHaveText(
      "How will you use Qubox?",
    );
    await expect($("[data-testid='mode-cloud']")).toBeDisplayed();
    await expect($("[data-testid='mode-selfhost']")).toBeDisplayed();
  });

  it("cloud path validates enroll code", async () => {
    await $("[data-testid='mode-cloud']").click();
    await $("[data-testid='first-run-details']").waitForExist();
    await expect($("[data-testid='first-run-details-title']")).toHaveText(
      "Finish setup",
    );

    await $("[data-testid='device-name']").setValue("E2E Laptop");
    await $("[data-testid='enroll-code']").setValue("SHORT");
    await $("[data-testid='first-run-continue']").click();

    await $("[data-testid='first-run-error']").waitForExist({ timeout: 10_000 });
    const err = await $("[data-testid='first-run-error']").getText();
    expect(err.toLowerCase()).toMatch(/enroll|code/);
  });

  it("self-host path completes with mocked backend", async () => {
    const complete = await browser.tauri.mock("complete_onboarding");
    await complete.mockResolvedValue(undefined);
    const setSetting = await browser.tauri.mock("set_setting");
    await setSetting.mockResolvedValue(undefined);

    await $("[data-testid='mode-selfhost']").click();
    await $("[data-testid='first-run-details']").waitForExist();
    await $("[data-testid='device-name']").setValue("E2E Selfhost");
    await $("[data-testid='first-run-continue']").click();

    await $("[data-testid='shell-app']").waitForExist({ timeout: 15_000 });
    await expect($("[data-testid='sidebar']")).toBeDisplayed();
    await expect($("[data-testid='nav-hosts']")).toBeDisplayed();

    await complete.update();
    expect(complete).toHaveBeenCalled();
  });
});
