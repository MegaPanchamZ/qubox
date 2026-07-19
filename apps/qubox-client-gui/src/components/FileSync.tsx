import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useApp } from "./AppContext";
import { open } from "@tauri-apps/plugin-dialog";
import {
  canAddIgnore,
  canQueuePush,
  mergeJobsWithDrain,
  splitCsvPaths,
} from "../lib/fileSyncLogic";

type SyncConflict = {
  conflictId: string;
  fileId: string;
  localPath: string;
  remotePath: string;
  peerId: string;
  createdAtUnix: number;
  localSize?: number;
  localModified?: number;
  remoteSize?: number;
  remoteModified?: number;
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

type SyncJob = {
  jobId: string;
  fileId: string;
  targetPeer: string;
  status: string;
  retryCount: number;
};

function formatBytes(bytes?: number): string {
  if (bytes === undefined) return "unknown size";
  if (bytes === 0) return "0 Bytes";
  const k = 1024;
  const sizes = ["Bytes", "KB", "MB", "GB"];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + " " + sizes[i];
}

function formatDate(unix?: number): string {
  if (unix === undefined) return "unknown date";
  return new Date(unix * 1000).toLocaleString();
}

export function FileSyncView() {
  const { knownHosts, setConflictCount } = useApp();
  const [ignores, setIgnores] = useState<string[]>([]);
  const [newIgnore, setNewIgnore] = useState("");
  const [conflicts, setConflicts] = useState<SyncConflict[]>([]);
  const [rules, setRules] = useState<SyncRule[]>([]);
  const [jobs, setJobs] = useState<SyncJob[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [ok, setOk] = useState<string | null>(null);
  const [rulePath, setRulePath] = useState("");
  const [rulePeer, setRulePeer] = useState("");
  const [ruleProcess, setRuleProcess] = useState("");
  const [pushPath, setPushPath] = useState("");
  const [pushPeer, setPushPeer] = useState("");

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

  const refresh = useCallback(async () => {
    setError(null);
    try {
      const [ig, cf, rl, jb, drain] = await Promise.all([
        invoke<string[]>("sync_list_ignores"),
        invoke<SyncConflict[]>("sync_list_conflicts"),
        invoke<SyncRule[]>("sync_list_rules"),
        invoke<SyncJob[]>("sync_list_jobs"),
        invoke<SyncJob[]>("sync_drain_ready").catch(() => [] as SyncJob[]),
      ]);
      setIgnores(ig);
      setConflicts(cf);
      setConflictCount(cf.length);
      setRules(rl);
      setJobs(mergeJobsWithDrain(jb, drain) as SyncJob[]);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

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
      setOk(`Applied preset: ${name}`);
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
      await invoke("sync_add_rule", {
        paths: splitCsvPaths(rulePath),
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

  return (
    <div className="view">
      <header className="view__header">
        <div>
          <p className="eyebrow">File Sync</p>
          <h1>Context-aware sync</h1>
          <p className="subtitle">
            Never-track patterns (defaults include <code>.git</code>), process-locked
            rules, outbox jobs, and binary conflict resolution. Files sync over
            paired QUIC sessions only — not the cloud.
          </p>
        </div>
        <button className="secondary-button" onClick={() => void refresh()} type="button">
          <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>refresh</span>
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
            presets for emulator saves or dev trees.
          </p>
          <div style={{ display: "flex", gap: 8, flexWrap: "wrap", marginBottom: 8 }}>
            <button className="secondary-button" onClick={() => void applyPreset("default")} type="button">
              <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>bookmark</span>
              Preset: default
            </button>
            <button className="secondary-button" onClick={() => void applyPreset("git")} type="button">
              <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>bookmark</span>
              Preset: git
            </button>
            <button
              className="secondary-button"
              onClick={() => void applyPreset("emulator-saves")}
              type="button"
            >
              <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>bookmark</span>
              Preset: emulator-saves
            </button>
            <button className="secondary-button" onClick={() => void applyPreset("dev")} type="button">
              <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>bookmark</span>
              Preset: dev
            </button>
          </div>
          <p className="subtitle" style={{ margin: "4px 0 12px", fontSize: "0.82rem", opacity: 0.8, lineHeight: "1.4" }}>
            <strong>Preset contents:</strong><br />
            • <code>default</code>: VCS paths, node_modules, target, build output, temp files.<br />
            • <code>git</code>: Excludes only <code>.git</code> repository folders.<br />
            • <code>emulator-saves</code>: Excludes ROMs and archives (<code>*.gba, *.nes, *.rom, *.iso</code>) but keeps saves.<br />
            • <code>dev</code>: Excludes dependency trees and build artifacts (<code>node_modules, target, .venv, __pycache__, *.o</code>).
          </p>
          <ul className="host-list" style={{ marginBottom: 12 }}>
            {ignores.length === 0 ? (
              <li className="state">No global ignore patterns.</li>
            ) : (
              ignores.map((p) => (
                <li key={p} className="host-card" style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
                  <code>{p}</code>
                  <button className="secondary-button" onClick={() => void removeIgnore(p)} type="button">
                    <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>delete</span>
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
            <button className="secondary-button" onClick={() => void addIgnore()} type="button">
              <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>add</span>
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
                const localNewer = c.localModified && c.remoteModified ? c.localModified > c.remoteModified : false;
                const remoteNewer = c.localModified && c.remoteModified ? c.remoteModified > c.localModified : false;
                return (
                  <li key={c.conflictId} className="host-card" style={{ display: "flex", flexDirection: "column", gap: 12 }}>
                    <div>
                      <strong style={{ fontSize: "1.05rem", color: "var(--primary)" }}>Conflict: {c.conflictId}</strong>
                      <div className="subtitle" style={{ marginTop: 4 }}>
                        Peer: <code>{c.peerId}</code> · Detected: {formatDate(c.createdAtUnix)}
                      </div>
                    </div>

                    <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 16, backgroundColor: "rgba(255, 255, 255, 0.02)", padding: 12, borderRadius: 6 }}>
                      <div style={{ borderRight: "1px solid rgba(229, 226, 221, 0.08)", paddingRight: 16 }}>
                        <span style={{ fontWeight: 600, display: "flex", alignItems: "center", gap: 6, fontSize: "0.9rem" }}>
                          Local Copy
                          {localNewer ? <span style={{ fontSize: "0.7rem", backgroundColor: "var(--primary)", color: "#000", padding: "1px 5px", borderRadius: 3, fontWeight: "bold" }}>NEWER</span> : null}
                        </span>
                        <div style={{ fontSize: "0.8rem", marginTop: 4, wordBreak: "break-all", opacity: 0.9, lineHeight: "1.4" }}>
                          Path: <code>{c.localPath}</code>
                          <br />
                          Size: {formatBytes(c.localSize)}
                          <br />
                          Modified: {formatDate(c.localModified)}
                        </div>
                      </div>

                      <div style={{ paddingLeft: 8 }}>
                        <span style={{ fontWeight: 600, display: "flex", alignItems: "center", gap: 6, fontSize: "0.9rem" }}>
                          Remote Copy (Quarantined)
                          {remoteNewer ? <span style={{ fontSize: "0.7rem", backgroundColor: "var(--primary)", color: "#000", padding: "1px 5px", borderRadius: 3, fontWeight: "bold" }}>NEWER</span> : null}
                        </span>
                        <div style={{ fontSize: "0.8rem", marginTop: 4, wordBreak: "break-all", opacity: 0.9, lineHeight: "1.4" }}>
                          Path: <code>{c.remotePath}</code>
                          <br />
                          Size: {formatBytes(c.remoteSize)}
                          <br />
                          Modified: {formatDate(c.remoteModified)}
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
                         onClick={() => void resolve(c.conflictId, "keep-remote")}
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
                     <p className="subtitle">Keep both retains the local file and the quarantined remote copy.</p>
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
              <li className="state">No directories monitored. Browse to add one.</li>
            ) : (
              rules.map((r) => (
                <li key={r.ruleId} className="host-card">
                  <code>{r.ruleId}</code> {r.enabled ? "on" : "off"}
                  <div className="subtitle">
                    paths={r.paths.join(", ")} · processes={r.processNames.join(", ")} · peers=
                    {r.peerIds.join(", ")}
                  </div>
                  <div className="subtitle">
                    max file size={formatBytes(r.maxFileBytes)} · ignores={r.ignoreGlobs.join(", ") || "global only"}
                  </div>
                </li>
              ))
            )}
          </ul>
          <p className="subtitle">Global ignore rules automatically apply to every watch rule.</p>
          <div style={{ display: "flex", gap: 8, marginBottom: 8 }}>
            <input
              className="text-input"
              onChange={(e) => setRulePath(e.target.value)}
              placeholder="Watch path(s), comma-separated"
              value={rulePath}
              style={{ flex: 1 }}
            />
            <button className="secondary-button" onClick={() => void pickFolder()} type="button">
              <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>folder_open</span>
              Browse
            </button>
          </div>
          <div style={{ display: "flex", flexDirection: "column", gap: 6, marginBottom: 8 }}>
            <select
              className="text-input"
              value={rulePeer}
              onChange={(e) => setRulePeer(e.target.value)}
              style={{ backgroundColor: "var(--bg-accent)", color: "var(--text)" }}
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
          <button className="secondary-button" onClick={() => void addRule()} type="button">
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>add</span>
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
            <button className="secondary-button" onClick={() => void pickFile()} type="button">
              <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>file_open</span>
              Browse
            </button>
          </div>
          <div style={{ display: "flex", flexDirection: "column", gap: 6, marginBottom: 8 }}>
            <select
              className="text-input"
              value={pushPeer}
              onChange={(e) => setPushPeer(e.target.value)}
              style={{ backgroundColor: "var(--bg-accent)", color: "var(--text)" }}
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
          <button className="secondary-button" onClick={() => void pushNow()} type="button">
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>send</span>
            Queue push
          </button>
        </div>

        <div className="settings-field">
          <span>Outbox jobs</span>
          {jobs.length === 0 ? (
            <p className="state">Empty</p>
          ) : (
            <ul className="host-list">
              {jobs.map((j) => (
                <li key={j.jobId} className="host-card">
                  {j.jobId} · <span title={j.status} style={{ display: "inline-block", maxWidth: "18rem", overflow: "hidden", textOverflow: "ellipsis", verticalAlign: "bottom", whiteSpace: "nowrap" }}>{j.status}</span> → {j.targetPeer} (retries {j.retryCount})
                </li>
              ))}
            </ul>
          )}
        </div>
      </section>
    </div>
  );
}
