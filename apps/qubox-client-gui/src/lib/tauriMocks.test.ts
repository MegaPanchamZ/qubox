import { afterEach, beforeAll, expect, test, vi } from "vitest";
import { randomFillSync } from "crypto";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { invoke } from "@tauri-apps/api/core";

beforeAll(() => {
  Object.defineProperty(globalThis, "crypto", {
    value: {
      getRandomValues: (buffer: Uint8Array) => randomFillSync(buffer),
    },
  });
});

afterEach(() => {
  clearMocks();
});

test("mockIPC intercepts get_onboarding", async () => {
  mockIPC((cmd) => {
    if (cmd === "get_onboarding") {
      return { completed: false, deviceName: "", signalingServer: "" };
    }
  });

  const spy = vi.spyOn(
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (window as any).__TAURI_INTERNALS__,
    "invoke",
  );

  await expect(invoke("get_onboarding")).resolves.toMatchObject({
    completed: false,
  });
  expect(spy).toHaveBeenCalled();
});

test("mockIPC can simulate cloud_enroll success", async () => {
  mockIPC((cmd, args) => {
    if (cmd === "cloud_enroll") {
      const a = args as { code?: string };
      return { ok: true, deviceId: "dev-1", code: a.code };
    }
  });

  await expect(
    invoke("cloud_enroll", { code: "ABCDEFGH", displayName: "Test" }),
  ).resolves.toMatchObject({ ok: true });
});
