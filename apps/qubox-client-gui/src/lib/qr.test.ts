import { describe, expect, it } from "vitest";
import { shareCodeSvg, shareLinkPayload, simpleCodeMatrix } from "./qr";

describe("qr", () => {
  it("builds share link", () => {
    expect(shareLinkPayload("ABC123")).toContain("code=ABC123");
  });

  it("builds matrix and svg", () => {
    const m = simpleCodeMatrix("test-code");
    expect(m.length).toBe(21);
    expect(m[0][0]).toBe(true); // finder
    const svg = shareCodeSvg("test-code");
    expect(svg.startsWith("<svg")).toBe(true);
    expect(svg.includes("rect")).toBe(true);
  });
});
