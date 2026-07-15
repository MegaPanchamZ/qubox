import { describe, expect, it } from "vitest";
import {
  CLOUD_SIGNALING,
  DEFAULT_SELFHOST_SIGNALING,
  isValidEnrollCode,
  normalizeEnrollCode,
  resolveAccountsUrl,
  resolveSignalingServer,
} from "./firstRun";

describe("firstRun", () => {
  it("normalizes enroll codes", () => {
    expect(normalizeEnrollCode(" ab-cd12 ")).toBe("ABCD12");
  });

  it("validates enroll code length", () => {
    expect(isValidEnrollCode("ABC")).toBe(false);
    expect(isValidEnrollCode("KXRETMZS")).toBe(true);
  });

  it("resolves cloud vs self-host signaling", () => {
    expect(resolveSignalingServer("cloud", "ws://x")).toBe(CLOUD_SIGNALING);
    expect(resolveSignalingServer("selfhost", "")).toBe(
      DEFAULT_SELFHOST_SIGNALING,
    );
    expect(resolveSignalingServer("selfhost", "ws://lan:7000/ws")).toBe(
      "ws://lan:7000/ws",
    );
  });

  it("accounts URL only for cloud", () => {
    expect(resolveAccountsUrl("cloud")).toMatch(/signal\.qubox\.app/);
    expect(resolveAccountsUrl("selfhost")).toBeNull();
  });
});
