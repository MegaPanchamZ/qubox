import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
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

export function FileSyncView() {
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
              Preset: default
            </button>
            <button className="secondary-button" onClick={() => void applyPreset("git")} type="button">
              Preset: git
            </button>
            <button
              className="secondary-button"
              onClick={() => void applyPreset("emulator-saves")}
              type="button"
            >
              Preset: emulator-saves
            </button>
            <button className="secondary-button" onClick={() => void applyPreset("dev")} type="button">
              Preset: dev
            </button>
          </div>
          <ul className="host-list" style={{ marginBottom: 12 }}>
            {ignores.map((p) => (
              <li key={p} className="host-card" style={{ display: "flex", justifyContent: "space-between" }}>
                <code>{p}</code>
                <button className="secondary-button" onClick={() => void removeIgnore(p)} type="button">
                  Remove
                </button>
              </li>
            ))}
          </ul>
          <div style={{ display: "flex", gap: 8 }}>
            <input
              className="text-input"
              onChange={(e) => setNewIgnore(e.target.value)}
              placeholder=".git, *.rom, node_modules, …"
              value={newIgnore}
            />
            <button className="secondary-button" onClick={() => void addIgnore()} type="button">
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
              {conflicts.map((c) => (
                <li key={c.conflictId} className="host-card">
                  <div>
                    <strong>{c.conflictId}</strong>
                    <div className="subtitle">
                      local: {c.localPath}
                      <br />
                      remote: {c.remotePath}
                      <br />
                      peer: {c.peerId}
                    </div>
                  </div>
                  <div style={{ display: "flex", gap: 6, marginTop: 8 }}>
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
                </li>
              ))}
            </ul>
          )}
        </div>

        <div className="settings-field">
          <span>Watch rules</span>
          <ul className="host-list">
            {rules.map((r) => (
              <li key={r.ruleId} className="host-card">
                <code>{r.ruleId}</code> {r.enabled ? "on" : "off"}
                <div className="subtitle">
                  paths={r.paths.join(", ")} · processes={r.processNames.join(", ")} · peers=
                  {r.peerIds.join(", ")}
                </div>
              </li>
            ))}
          </ul>
          <input
            className="text-input"
            onChange={(e) => setRulePath(e.target.value)}
            placeholder="Watch path(s), comma-separated"
            value={rulePath}
          />
          <input
            className="text-input"
            onChange={(e) => setRulePeer(e.target.value)}
            placeholder="Peer id(s)"
            value={rulePeer}
          />
          <input
            className="text-input"
            onChange={(e) => setRuleProcess(e.target.value)}
            placeholder="Process lock names (e.g. mgba)"
            value={ruleProcess}
          />
          <button className="secondary-button" onClick={() => void addRule()} type="button">
            Add rule
          </button>
        </div>

        <div className="settings-field">
          <span>Manual push</span>
          <input
            className="text-input"
            onChange={(e) => setPushPath(e.target.value)}
            placeholder="/path/to/file.sav"
            value={pushPath}
          />
          <input
            className="text-input"
            onChange={(e) => setPushPeer(e.target.value)}
            placeholder="target peer id"
            value={pushPeer}
          />
          <button className="secondary-button" onClick={() => void pushNow()} type="button">
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
                  {j.jobId} · {j.status} → {j.targetPeer} (retries {j.retryCount})
                </li>
              ))}
            </ul>
          )}
        </div>
      </section>
    </div>
  );
}
