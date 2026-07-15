/** Minimal QR payload helpers (code string + optional data URL via canvas). */

export function shareLinkPayload(code: string, baseUrl?: string): string {
  const base = (baseUrl ?? "https://qubox.app/pair").replace(/\/$/, "");
  return `${base}?code=${encodeURIComponent(code)}`;
}

/** Render a QR matrix as SVG (no native image dependency). */
export function qrMatrixToSvg(matrix: boolean[][], cell = 4, margin = 2): string {
  const n = matrix.length;
  const size = (n + margin * 2) * cell;
  let rects = "";
  for (let y = 0; y < n; y++) {
    for (let x = 0; x < n; x++) {
      if (matrix[y][x]) {
        rects += `<rect x="${(x + margin) * cell}" y="${(y + margin) * cell}" width="${cell}" height="${cell}" fill="#000"/>`;
      }
    }
  }
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 ${size} ${size}"><rect width="100%" height="100%" fill="#fff"/>${rects}</svg>`;
}

/**
 * Tiny QR-ish placeholder matrix from a string (not full ISO QR).
 * Good enough for UI demos; production can swap for a real encoder.
 */
export function simpleCodeMatrix(code: string, size = 21): boolean[][] {
  const m: boolean[][] = Array.from({ length: size }, () =>
    Array.from({ length: size }, () => false),
  );
  // Finder patterns
  for (const [ox, oy] of [
    [0, 0],
    [size - 7, 0],
    [0, size - 7],
  ] as const) {
    for (let y = 0; y < 7; y++) {
      for (let x = 0; x < 7; x++) {
        const edge = x === 0 || y === 0 || x === 6 || y === 6;
        const core = x >= 2 && x <= 4 && y >= 2 && y <= 4;
        m[oy + y][ox + x] = edge || core;
      }
    }
  }
  // Data bits from code hash
  let h = 0;
  for (let i = 0; i < code.length; i++) h = (h * 33 + code.charCodeAt(i)) >>> 0;
  for (let y = 8; y < size - 8; y++) {
    for (let x = 8; x < size - 8; x++) {
      h = (h * 1103515245 + 12345) >>> 0;
      m[y][x] = (h & 1) === 1;
    }
  }
  return m;
}

export function shareCodeSvg(code: string): string {
  return qrMatrixToSvg(simpleCodeMatrix(code));
}
