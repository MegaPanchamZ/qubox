import { describe, expect, it } from "vitest";
import { codeMatrix, shareCodeSvg, shareLinkPayload } from "./qr";

describe("qr", () => {
  it("builds share link", () => {
    expect(shareLinkPayload("ABC123")).toContain("code=ABC123");
  });

  it("encodes a real QR matrix", () => {
    const m = codeMatrix("test-code");
    expect(m.length).toBeGreaterThan(0);
    expect(m.length).toBe(m[0].length);
    // QR finder patterns occupy the top-left, top-right, and bottom-left corners
    expect(m[0][0]).toBe(true);
    expect(m[0][m.length - 1]).toBe(true);
    expect(m[m.length - 1][0]).toBe(true);
    // White quiet zone between finder and data — first interior cell of finder row is dark,
    // second is white (separator).
    expect(m[0][7]).toBe(false);
  });

  it("renders matrix as svg", () => {
    const svg = shareCodeSvg("test-code");
    expect(svg.startsWith("<svg")).toBe(true);
    expect(svg.includes("<rect")).toBe(true);
  });

  it("produces a different matrix for different inputs", () => {
    const a = codeMatrix("alpha");
    const b = codeMatrix("beta");
    // Two distinct strings should not produce byte-identical matrices
    expect(JSON.stringify(a)).not.toBe(JSON.stringify(b));
  });
});
