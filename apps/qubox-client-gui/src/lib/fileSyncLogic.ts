/** Pure File Sync UI helpers (unit-tested). */

export function splitCsvPaths(input: string): string[] {
  return input
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
}

export function canAddIgnore(pattern: string): boolean {
  return pattern.trim().length > 0;
}

export function canQueuePush(localPath: string, targetPeer: string): boolean {
  return localPath.trim().length > 0 && targetPeer.trim().length > 0;
}

export function mergeJobsWithDrain(
  jobs: { jobId: string }[],
  drain: { jobId: string }[],
): { jobId: string }[] {
  if (jobs.length > 0) return jobs;
  return drain;
}

export function ignoreContainsGit(patterns: string[]): boolean {
  return patterns.some(
    (p) => p === ".git" || p.endsWith("/.git") || p.includes(".git"),
  );
}

export type ConflictResolution = "keep-local" | "keep-remote" | "keep-both";

export function isConflictResolution(v: string): v is ConflictResolution {
  return v === "keep-local" || v === "keep-remote" || v === "keep-both";
}
