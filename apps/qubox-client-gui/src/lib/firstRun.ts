/** Pure helpers for first-run Cloud vs self-host (unit-tested). */

export type SetupMode = "cloud" | "selfhost";

export const CLOUD_SIGNALING = "wss://signal.qubox.app/ws";
export const CLOUD_ACCOUNTS = "https://signal.qubox.app";
export const DEFAULT_SELFHOST_SIGNALING = "ws://127.0.0.1:7000/ws";

export function normalizeEnrollCode(raw: string): string {
  return raw.trim().toUpperCase().replace(/[^A-Z0-9]/g, "");
}

export function isValidEnrollCode(code: string): boolean {
  const c = normalizeEnrollCode(code);
  return c.length >= 6 && c.length <= 16;
}

export function resolveSignalingServer(
  mode: SetupMode,
  selfhostUrl: string,
): string {
  if (mode === "cloud") return CLOUD_SIGNALING;
  const t = selfhostUrl.trim();
  return t || DEFAULT_SELFHOST_SIGNALING;
}

export function resolveAccountsUrl(mode: SetupMode): string | null {
  return mode === "cloud" ? CLOUD_ACCOUNTS : null;
}
