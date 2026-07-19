/** Pure helpers for first-run Cloud vs self-host (unit-tested). */

export type SetupMode = "cloud" | "selfhost";

export const CLOUD_SIGNALING = "wss://signal.qubox.app/ws";
export const CLOUD_ACCOUNTS = "https://signal.qubox.app";
/** Reserved for backwards compat — new code should leave the
 *  self-host URL blank by default to force the user to type a
 *  routable address instead of poisoning pairings with `127.0.0.1`. */
export const DEFAULT_SELFHOST_SIGNALING = "";

export function normalizeEnrollCode(raw: string): string {
  return raw
    .trim()
    .toUpperCase()
    .replace(/[^A-Z0-9]/g, "");
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
  return selfhostUrl.trim();
}

export function resolveAccountsUrl(mode: SetupMode): string | null {
  return mode === "cloud" ? CLOUD_ACCOUNTS : null;
}
