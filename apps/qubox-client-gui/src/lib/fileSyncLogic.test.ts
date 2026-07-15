import { describe, expect, it } from "vitest";
import {
  canAddIgnore,
  canQueuePush,
  ignoreContainsGit,
  isConflictResolution,
  mergeJobsWithDrain,
  splitCsvPaths,
} from "./fileSyncLogic";

describe("fileSyncLogic", () => {
  it("splitCsvPaths trims and drops empties", () => {
    expect(splitCsvPaths(" a , b,, c ")).toEqual(["a", "b", "c"]);
    expect(splitCsvPaths("")).toEqual([]);
  });

  it("canAddIgnore requires non-empty pattern", () => {
    expect(canAddIgnore("  ")).toBe(false);
    expect(canAddIgnore(".git")).toBe(true);
  });

  it("canQueuePush needs path and peer", () => {
    expect(canQueuePush("/tmp/x.sav", "peer")).toBe(true);
    expect(canQueuePush("", "peer")).toBe(false);
    expect(canQueuePush("/tmp/x.sav", "")).toBe(false);
  });

  it("mergeJobsWithDrain prefers jobs list", () => {
    expect(mergeJobsWithDrain([{ jobId: "a" }], [{ jobId: "b" }])).toEqual([
      { jobId: "a" },
    ]);
    expect(mergeJobsWithDrain([], [{ jobId: "b" }])).toEqual([{ jobId: "b" }]);
  });

  it("ignoreContainsGit detects default never-track", () => {
    expect(ignoreContainsGit(["node_modules", ".git"])).toBe(true);
    expect(ignoreContainsGit(["*.tmp"])).toBe(false);
  });

  it("isConflictResolution", () => {
    expect(isConflictResolution("keep-local")).toBe(true);
    expect(isConflictResolution("drop")).toBe(false);
  });
});
