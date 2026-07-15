import { describe, expect, it } from "vitest";
import {
  isPrivacyMode,
  isStreamMode,
  normalizePrivacyMode,
  normalizeStreamMode,
  privacyCliFlags,
} from "./hostPrefs";

describe("hostPrefs", () => {
  it("validates privacy modes", () => {
    expect(isPrivacyMode("blank-overlay")).toBe(true);
    expect(isPrivacyMode("vkms")).toBe(true);
    expect(isPrivacyMode("none")).toBe(true);
    expect(isPrivacyMode("invalid")).toBe(false);
  });

  it("validates stream modes", () => {
    expect(isStreamMode("multi-display")).toBe(true);
    expect(isStreamMode("single-stream")).toBe(true);
    expect(isStreamMode("all-displays")).toBe(true);
    expect(isStreamMode("tiled")).toBe(false);
  });

  it("privacyCliFlags enables on session start when not none", () => {
    expect(privacyCliFlags("none")).toEqual({
      privacyMode: "none",
      enableOnSessionStart: false,
    });
    expect(privacyCliFlags("blank-overlay").enableOnSessionStart).toBe(true);
    expect(privacyCliFlags("vkms").enableOnSessionStart).toBe(true);
  });

  it("normalizes unknown values to safe defaults", () => {
    expect(normalizePrivacyMode(undefined)).toBe("none");
    expect(normalizePrivacyMode("nope")).toBe("none");
    expect(normalizeStreamMode(null)).toBe("single-stream");
    expect(normalizeStreamMode("multi-display")).toBe("multi-display");
  });
});
