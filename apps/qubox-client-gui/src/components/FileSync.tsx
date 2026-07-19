import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { exists } from "@tauri-apps/plugin-fs";
import { useApp } from "./AppContext";
import {
  canAddIgnore,
  canQueuePush,
  splitCsvPaths,
} from "../lib/fileSyncLogic";

type SyncConflict = {
  conflictId: string;
  fileId: string;
  localPath: string;
  remotePath: string;
  peerId: string;
  createdAtMs: number;
  localSize?: number;
  localModifiedMs?: number;
  remoteSize?: number;
  remoteModifiedMs?: number;
};

type SyncRule = {
  ruleId: string;
  paths: string[];
  processNames: string[];
  peerIds: string[];
  enabled: boolean;
  maxFileBytes: number;
  ignoreGlobs: string[];
};

type OutboxJob = {
  jobId: string;
  fileId: string;
  targetPeer: string;
  status: "queued" | "in_flight" | "failed" | "done";
  retryCount: number;
  queuedAtMs?: number;
  lastError?: string | null;
};

type DashboardPayload = {
  ignores: string[];
  conflicts: SyncConflict[];
  rules: SyncRule[];
  jobs: OutboxJob[];
  drain: OutboxJob[];
};

const STATUS_LIMIT = 32;
const STATUS_FALLBACK: Record<OutboxJob["status"], string> = {
  queued: "queued",
  in_flight: "in flight",
  failed: "failed",
  done: "done",
};

function formatBytes(bytes?: number): string {
  if (bytes === undefined) return "unknown size";
  if (bytes === 0) return "0 Bytes";
  const k = 1024;
  const sizes = ["Bytes", "KB", "MB", "GB"];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + " " + sizes[i];
}

function formatDate(unixMs?: number): string {
  if (unixMs === undefined) return "unknown date";
  return new Date(unixMs).toLocaleString();
}

function truncateStatus(status: string | OutboxJob["status"]): string {
  const raw = typeof status === "string" ? status : STATUS_FALLBACK[status];
  if (raw.length <= STATUS_LIMIT) return raw;
  return `${raw.slice(0, STATUS_LIMIT - 1)}…`;
}

export function FileSyncView() {
  const { knownHosts, setConflictCount, notificationsAllowed } = useApp();
  const [ignores, setIgnores] = useState<string[]>([]);
  const [newIgnore, setNewIgnore] = useState("");
  const [conflicts, setConflicts] = useState<SyncConflict[]>([]);
  const [rules, setRules] = useState<SyncRule[]>([]);
  const [jobs, setJobs] = useState<OutboxJob[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [ok, setOk] = useState<string | null>(null);
  const [rulePath, setRulePath] = useState("");
  const [rulePeer, setRulePeer] = useState("");
  const [ruleProcess, setRuleProcess] = useState("");
  const [pushPath, setPushPath] = useState("");
  const [pushPeer, setPushPeer] = useState("");
  const [pushPathValid, setPushPathValid] = useState<boolean | null>(null);

  const refresh = useCallback(async () => {
    setError(null);
    try {
      const payload = await invoke<DashboardPayload>(
        "sync_get_dashboard_state",
      );
      setIgnores(payload.ignores ?? []);
      setConflicts(payload.conflicts ?? []);
      setRules(payload.rules ?? []);
      setConflictCount((payload.conflicts ?? []).length);
      const mergedJobs =
        (payload.jobs ?? []).length > 0 ? payload.jobs : (payload.drain ?? []);
      setJobs(mergedJobs);
    } catch (e) {
      setError(String(e));
    }
  }, [setConflictCount]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Native notification on new sync conflict (handled via daemon event).
  useEffect(() => {
    if (!notificationsAllowed) return;
    if (conflicts.length === 0) return;
    const last = conflicts[conflicts.length - 1];
    void invoke("notify_user", {
      title: "Qubox sync conflict",
      body: `Conflict on ${last.localPath.split(/[\\/]/).pop() ?? last.fileId}`,
    }).catch(() => {});
  }, [conflicts, notificationsAllowed]);

  // Validate manual push path via plugin-fs exists().
  useEffect(() => {
    let cancelled = false;
    if (pushPath.trim().length === 0) {
      setPushPathValid(null);
      return;
    }
    exists(pushPath)
      .then((ok) => {
        if (!cancelled) setPushPathValid(ok);
      })
      .catch(() => {
        if (!cancelled) setPushPathValid(false);
      });
    return () => {
      cancelled = true;
    };
  }, [pushPath]);

  const pickFolder = async () => {
    try {
      const selected = await open({
        directory: true,
        multiple: false,
        title: "Select Watch Directory",
      });
      if (typeof selected === "string") {
        setRulePath(selected);
      }
    } catch (err) {
      console.error(err);
    }
  };

  const pickFile = async () => {
    try {
      const selected = await open({
        directory: false,
        multiple: false,
        title: "Select File to Push",
      });
      if (typeof selected === "string") {
        setPushPath(selected);
      }
    } catch (err) {
      console.error(err);
    }
  };

  const addIgnore = async () => {
    if (!canAddIgnore(newIgnore)) return;
    try {
      const next = await invoke<string[]>("sync_add_ignore", {
        pattern: newIgnore.trim(),
      });
      setIgnores(next);
      setNewIgnore("");
      setOk(`Added ignore: ${newIgnore.trim()}`);
    } catch (e) {
      setError(String(e));
    }
  };

  const removeIgnore = async (pattern: string) => {
    try {
      const next = await invoke<string[]>("sync_remove_ignore", { pattern });
      setIgnores(next);
      setOk(`Removed ignore: ${pattern}`);
    } catch (e) {
      setError(String(e));
    }
  };

  const applyPreset = async (name: string) => {
    try {
      const next = await invoke<string[]>("sync_apply_ignore_preset", { name });
      setIgnores(next);
      setOk(`Applied preset: ${name} (merged with existing patterns)`);
    } catch (e) {
      setError(String(e));
    }
  };

  const resolve = async (
    conflictId: string,
    resolution: "keep-local" | "keep-remote" | "keep-both",
  ) => {
    try {
      await invoke("sync_resolve_conflict", { conflictId, resolution });
      setOk(`Resolved ${conflictId} → ${resolution}`);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const addRule = async () => {
    try {
      // Paths are taken as a single string to avoid breaking on commas
      // inside real directory names ("/Users/dev/Projects/App, Final").
      // Process names and peer IDs are domain identifiers and can still
      // be a comma-separated list.
      const paths = rulePath.trim() ? [rulePath.trim()] : [];
      await invoke("sync_add_rule", {
        paths,
        processNames: splitCsvPaths(ruleProcess),
        peerIds: splitCsvPaths(rulePeer),
        ignoreGlobs: [] as string[],
      });
      setOk("Rule added");
      setRulePath("");
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const pushNow = async () => {
    if (!canQueuePush(pushPath, pushPeer)) {
      setError("Push requires a local path and target peer");
      return;
    }
    if (pushPathValid === false) {
      setError(`Push path does not exist: ${pushPath}`);
      return;
    }
    try {
      await invoke("sync_push_now", {
        localPath: pushPath,
        targetPeer: pushPeer,
        nodeId: "gui",
      });
      setOk("Push queued");
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const retryJob = async (jobId: string) => {
    try {
      await invoke("sync_retry_job", { jobId });
      setOk(`Retry dispatched for ${jobId}`);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const dismissJob = async (jobId: string) => {
    try {
      await invoke("sync_dismiss_job", { jobId });
      setOk(`Dismissed ${jobId}`);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="view">
      <header className="view__header">
        <div>
          <p className="eyebrow">File Sync</p>
          <h1>Context-aware sync</h1>
          <p className="subtitle">
            Never-track patterns (defaults include <code>.git</code>),
            process-locked rules, outbox jobs, and binary conflict resolution.
            Files sync over paired QUIC sessions only — not the cloud.
          </p>
        </div>
        <button
          className="secondary-button"
          onClick={() => void refresh()}
          type="button"
        >
          <span
            className="material-symbols-outlined"
            style={{ fontSize: "1.1rem" }}
          >
            refresh
          </span>
          Refresh
        </button>
      </header>

      {error ? <p className="state state--error">{error}</p> : null}
      {ok ? <p className="state">{ok}</p> : null}

      <section className="settings-grid">
        <div className="settings-field">
          <span>Never track (global ignores)</span>
          <p className="subtitle" style={{ margin: "0 0 8px" }}>
            Path segments and globs. Always starts with <code>.git</code>. Use
            presets for emulator saves or dev trees. Presets append to your list
            so custom rules are preserved.
          </p>
          <div
            style={{
              display: "flex",
              gap: 8,
              flexWrap: "wrap",
              marginBottom: 8,
            }}
          >
            <button
              className="secondary-button"
              onClick={() => void applyPreset("default")}
              type="button"
            >
              <span
                className="material-symbols-outlined"
                style={{ fontSize: "1.1rem" }}
              >
                bookmark
              </span>
              Preset: default
            </button>
            <button
              className="secondary-button"
              onClick={() => void applyPreset("git")}
              type="button"
            >
              <span
                className="material-symbols-outlined"
                style={{ fontSize: "1.1rem" }}
              >
                bookmark
              </span>
              Preset: git
            </button>
            <button
              className="secondary-button"
              onClick={() => void applyPreset("emulator-saves")}
              type="button"
            >
              <span
                className="material-symbols-outlined"
                style={{ fontSize: "1.1rem" }}
              >
                bookmark
              </span>
              Preset: emulator-saves
            </button>
            <button
              className="secondary-button"
              onClick={() => void applyPreset("dev")}
              type="button"
            >
              <span
                className="material-symbols-outlined"
                style={{ fontSize: "1.1rem" }}
              >
                bookmark
              </span>
              Preset: dev
            </button>
          </div>
          <p
            className="subtitle"
            style={{
              margin: "4px 0 12px",
              fontSize: "0.82rem",
              opacity: 0.8,
              lineHeight: "1.4",
            }}
          >
            <strong>Preset contents:</strong>
            <br />• <code>default</code>: VCS paths, node_modules, target, build
            output, temp files.
            <br />• <code>git</code>: Excludes only <code>.git</code> repository
            folders.
            <br />• <code>emulator-saves</code>: Excludes ROMs and archives (
            <code>*.gba, *.nes, *.rom, *.iso</code>) but keeps saves.
            <br />• <code>dev</code>: Excludes dependency trees and build
            artifacts (
            <code>node_modules, target, .venv, __pycache__, *.o</code>).
          </p>
          <ul className="host-list" style={{ marginBottom: 12 }}>
            {ignores.length === 0 ? (
              <li className="state">No global ignore patterns.</li>
            ) : (
              ignores.map((p) => (
                <li
                  key={p}
                  className="host-card"
                  style={{
                    display: "flex",
                    justifyContent: "space-between",
                    alignItems: "center",
                  }}
                >
                  <code>{p}</code>
                  <button
                    className="secondary-button"
                    onClick={() => void removeIgnore(p)}
                    type="button"
                  >
                    <span
                      className="material-symbols-outlined"
                      style={{ fontSize: "1.1rem" }}
                    >
                      delete
                    </span>
                    Remove
                  </button>
                </li>
              ))
            )}
          </ul>
          <div style={{ display: "flex", gap: 8 }}>
            <input
              className="text-input"
              onChange={(e) => setNewIgnore(e.target.value)}
              placeholder=".git, *.rom, node_modules, …"
              value={newIgnore}
            />
            <button
              className="secondary-button"
              onClick={() => void addIgnore()}
              type="button"
            >
              <span
                className="material-symbols-outlined"
                style={{ fontSize: "1.1rem" }}
              >
                add
              </span>
              Add
            </button>
          </div>
        </div>

        <div className="settings-field">
          <span>Conflicts</span>
          {conflicts.length === 0 ? (
            <p className="state">No conflicts</p>
          ) : (
            <ul className="host-list">
              {conflicts.map((c) => {
                const localNewer =
                  c.localModifiedMs && c.remoteModifiedMs
                    ? c.localModifiedMs > c.remoteModifiedMs
                    : false;
                const remoteNewer =
                  c.localModifiedMs && c.remoteModifiedMs
                    ? c.remoteModifiedMs > c.localModifiedMs
                    : false;
                return (
                  <li
                    key={c.conflictId}
                    className="host-card"
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      gap: 12,
                    }}
                  >
                    <div>
                      <strong
                        style={{ fontSize: "1.05rem", color: "var(--primary)" }}
                      >
                        Conflict: {c.conflictId}
                      </strong>
                      <div className="subtitle" style={{ marginTop: 4 }}>
                        Peer: <code>{c.peerId}</code> · Detected:{" "}
                        {formatDate(c.createdAtMs)}
                      </div>
                    </div>

                    <div
                      style={{
                        display: "grid",
                        gridTemplateColumns: "1fr 1fr",
                        gap: 16,
                        backgroundColor: "rgba(255, 255, 255, 0.02)",
                        padding: 12,
                        borderRadius: 6,
                      }}
                    >
                      <div
                        style={{
                          borderRight: "1px solid rgba(229, 226, 221, 0.08)",
                          paddingRight: 16,
                        }}
                      >
                        <span
                          style={{
                            fontWeight: 600,
                            display: "flex",
                            alignItems: "center",
                            gap: 6,
                            fontSize: "0.9rem",
                          }}
                        >
                          Local Copy
                          {localNewer ? (
                            <span
                              style={{
                                fontSize: "0.7rem",
                                backgroundColor: "var(--primary)",
                                color: "#000",
                                padding: "1px 5px",
                                borderRadius: 3,
                                fontWeight: "bold",
                              }}
                            >
                              NEWER
                            </span>
                          ) : null}
                        </span>
                        <div
                          style={{
                            fontSize: "0.8rem",
                            marginTop: 4,
                            wordBreak: "break-all",
                            opacity: 0.9,
                            lineHeight: "1.4",
                          }}
                        >
                          Path: <code>{c.localPath}</code>
                          <br />
                          Size: {formatBytes(c.localSize)}
                          <br />
                          Modified: {formatDate(c.localModifiedMs)}
                        </div>
                      </div>

                      <div style={{ paddingLeft: 8 }}>
                        <span
                          style={{
                            fontWeight: 600,
                            display: "flex",
                            alignItems: "center",
                            gap: 6,
                            fontSize: "0.9rem",
                          }}
                        >
                          Remote Copy (Quarantined)
                          {remoteNewer ? (
                            <span
                              style={{
                                fontSize: "0.7rem",
                                backgroundColor: "var(--primary)",
                                color: "#000",
                                padding: "1px 5px",
                                borderRadius: 3,
                                fontWeight: "bold",
                              }}
                            >
                              NEWER
                            </span>
                          ) : null}
                        </span>
                        <div
                          style={{
                            fontSize: "0.8rem",
                            marginTop: 4,
                            wordBreak: "break-all",
                            opacity: 0.9,
                            lineHeight: "1.4",
                          }}
                        >
                          Path: <code>{c.remotePath}</code>
                          <br />
                          Size: {formatBytes(c.remoteSize)}
                          <br />
                          Modified: {formatDate(c.remoteModifiedMs)}
                        </div>
                      </div>
                    </div>

                    <div style={{ display: "flex", gap: 8, marginTop: 4 }}>
                      <button
                        className="secondary-button"
                        onClick={() => void resolve(c.conflictId, "keep-local")}
                        type="button"
                      >
                        Keep local
                      </button>
                      <button
                        className="secondary-button"
                        onClick={() =>
                          void resolve(c.conflictId, "keep-remote")
                        }
                        type="button"
                      >
                        Keep remote
                      </button>
                      <button
                        className="secondary-button"
                        onClick={() => void resolve(c.conflictId, "keep-both")}
                        type="button"
                      >
                        Keep both
                      </button>
                    </div>
                    <p className="subtitle">
                      Keep both keeps the local file at its original path and
                      renames the quarantined remote copy to{" "}
                      <code>{`{name} (remote).{ext}`}</code>.
                    </p>
                  </li>
                );
              })}
            </ul>
          )}
        </div>

        <div className="settings-field">
          <span>Watch rules</span>
          <ul className="host-list">
            {rules.length === 0 ? (
              <li className="state">
                No directories monitored. Browse to add one.
              </li>
            ) : (
              rules.map((r) => (
                <li key={r.ruleId} className="host-card">
                  <code>{r.ruleId}</code> {r.enabled ? "on" : "off"}
                  <div className="subtitle">
                    paths={r.paths.join(", ")} · processes=
                    {r.processNames.join(", ")} · peers=
                    {r.peerIds.join(", ")}
                  </div>
                  <div className="subtitle">
                    max file size={formatBytes(r.maxFileBytes)} · ignores=
                    {r.ignoreGlobs.join(", ") || "global only"}
                  </div>
                </li>
              ))
            )}
          </ul>
          <p className="subtitle">
            Global ignore rules automatically apply to every watch rule. Files
            larger than the rule's <code>max file size</code> are skipped by the
            sync engine.
          </p>
          <div style={{ display: "flex", gap: 8, marginBottom: 8 }}>
            <input
              className="text-input"
              onChange={(e) => setRulePath(e.target.value)}
              placeholder="Watch path (use Browse for directory picker)"
              value={rulePath}
              style={{ flex: 1 }}
            />
            <button
              className="secondary-button"
              onClick={() => void pickFolder()}
              type="button"
            >
              <span
                className="material-symbols-outlined"
                style={{ fontSize: "1.1rem" }}
              >
                folder_open
              </span>
              Browse
            </button>
          </div>
          <div
            style={{
              display: "flex",
              flexDirection: "column",
              gap: 6,
              marginBottom: 8,
            }}
          >
            <select
              className="text-input"
              value={rulePeer}
              onChange={(e) => setRulePeer(e.target.value)}
              style={{
                backgroundColor: "var(--bg-accent)",
                color: "var(--text)",
              }}
            >
              <option value="">-- Select Peer ID from Paired Hosts --</option>
              {knownHosts.map((h) => (
                <option key={h.hostPeerId} value={h.hostPeerId}>
                  {h.displayName || "Unnamed Peer"} ({h.hostPeerId})
                </option>
              ))}
            </select>
            <input
              className="text-input"
              onChange={(e) => setRulePeer(e.target.value)}
              placeholder="Or enter custom peer ID manually"
              value={rulePeer}
            />
          </div>
          <input
            className="text-input"
            onChange={(e) => setRuleProcess(e.target.value)}
            placeholder="Process lock names (e.g. mgba)"
            value={ruleProcess}
            style={{ marginBottom: 8 }}
          />
          <button
            className="secondary-button"
            onClick={() => void addRule()}
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
              add
            </span>
            Add rule
          </button>
        </div>

        <div className="settings-field">
          <span>Manual push</span>
          <div style={{ display: "flex", gap: 8, marginBottom: 8 }}>
            <input
              className="text-input"
              onChange={(e) => setPushPath(e.target.value)}
              placeholder="/path/to/file.sav"
              value={pushPath}
              style={{ flex: 1 }}
            />
            <button
              className="secondary-button"
              onClick={() => void pickFile()}
              type="button"
            >
              <span
                className="material-symbols-outlined"
                style={{ fontSize: "1.1rem" }}
              >
                file_open
              </span>
              Browse
            </button>
          </div>
          {pushPathValid === false ? (
            <p className="state state--error" style={{ fontSize: "0.8rem" }}>
              Path does not exist on this machine.
            </p>
          ) : null}
          <div
            style={{
              display: "flex",
              flexDirection: "column",
              gap: 6,
              marginBottom: 8,
            }}
          >
            <select
              className="text-input"
              value={pushPeer}
              onChange={(e) => setPushPeer(e.target.value)}
              style={{
                backgroundColor: "var(--bg-accent)",
                color: "var(--text)",
              }}
            >
              <option value="">-- Select Target Peer ID --</option>
              {knownHosts.map((h) => (
                <option key={h.hostPeerId} value={h.hostPeerId}>
                  {h.displayName || "Unnamed Peer"} ({h.hostPeerId})
                </option>
              ))}
            </select>
            <input
              className="text-input"
              onChange={(e) => setPushPeer(e.target.value)}
              placeholder="Or enter custom target peer ID manually"
              value={pushPeer}
            />
          </div>
          <button
            className="secondary-button"
            disabled={
              !canQueuePush(pushPath, pushPeer) || pushPathValid === false
            }
            onClick={() => void pushNow()}
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
              send
            </span>
            Queue push
          </button>
        </div>

        <div className="settings-field">
          <span>Outbox jobs</span>
          {jobs.length === 0 ? (
            <p className="state">Empty</p>
          ) : (
            <ul className="host-list">
              {jobs.map((j) => {
                const statusText = truncateStatus(j.status);
                const isTerminal = j.status === "failed" || j.status === "done";
                return (
                  <li
                    key={j.jobId}
                    className="host-card"
                    style={{ display: "flex", flexDirection: "column", gap: 6 }}
                  >
                    <div
                      style={{
                        display: "flex",
                        justifyContent: "space-between",
                        gap: 8,
                      }}
                    >
                      <span
                        style={{
                          overflow: "hidden",
                          textOverflow: "ellipsis",
                          whiteSpace: "nowrap",
                        }}
                      >
                        <code>{j.jobId}</code> · {statusText} → {j.targetPeer}{" "}
                        (retries {j.retryCount})
                      </span>
                      {isTerminal ? (
                        <div style={{ display: "flex", gap: 6 }}>
                          <button
                            className="secondary-button"
                            onClick={() => void retryJob(j.jobId)}
                            type="button"
                          >
                            Retry
                          </button>
                          <button
                            className="secondary-button"
                            onClick={() => void dismissJob(j.jobId)}
                            type="button"
                          >
                            Dismiss
                          </button>
                        </div>
                      ) : null}
                    </div>
                    {j.lastError ? (
                      <p
                        className="state state--error"
                        style={{ fontSize: "0.8rem" }}
                      >
                        last error: {truncateStatus(j.lastError)}
                      </p>
                    ) : null}
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      </section>
    </div>
  );
}
