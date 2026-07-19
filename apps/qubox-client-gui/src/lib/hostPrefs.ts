/** Pure helpers for Host mode privacy / stream prefs (unit-tested). */

export type PrivacyMode = "none" | "blank-overlay" | "vkms";
export type StreamMode = "single-stream" | "multi-display" | "all-displays";

export function isPrivacyMode(value: string): value is PrivacyMode {
  return value === "none" || value === "blank-overlay" || value === "vkms";
}

export function isStreamMode(value: string): value is StreamMode {
  return (
    value === "single-stream" ||
    value === "multi-display" ||
    value === "all-displays"
  );
}

/** Host CLI flags implied by a stored privacy preference. */
export function privacyCliFlags(mode: PrivacyMode): {
  privacyMode: PrivacyMode;
  enableOnSessionStart: boolean;
} {
  return {
    privacyMode: mode,
    enableOnSessionStart: mode !== "none",
  };
}

export function normalizePrivacyMode(
  raw: string | null | undefined,
): PrivacyMode {
  if (raw && isPrivacyMode(raw)) return raw;
  return "none";
}

export function normalizeStreamMode(
  raw: string | null | undefined,
): StreamMode {
  if (raw && isStreamMode(raw)) return raw;
  return "single-stream";
}
