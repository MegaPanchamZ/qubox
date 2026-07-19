/** QR payload helpers — encoder + SVG renderer. */

import qrcode from "qrcode";

export function shareLinkPayload(code: string, baseUrl?: string): string {
  const base = (baseUrl ?? "https://qubox.app/pair").replace(/\/$/, "");
  return `${base}?code=${encodeURIComponent(code)}`;
}

/** Encode `text` to a boolean matrix using a real ISO/IEC 18004 QR encoder. */
export function codeMatrix(text: string): boolean[][] {
  const qr = qrcode.create(text, { errorCorrectionLevel: "M" });
  const size = qr.modules.size;
  const data = qr.modules.data;
  const matrix: boolean[][] = [];
  for (let y = 0; y < size; y++) {
    const row: boolean[] = [];
    for (let x = 0; x < size; x++) {
      row.push(Boolean(data[y * size + x]));
    }
    matrix.push(row);
  }
  return matrix;
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

export function shareCodeSvg(code: string): string {
  return qrMatrixToSvg(codeMatrix(code));
}
