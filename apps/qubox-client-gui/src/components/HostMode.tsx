import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { SharePanel } from "./SharePanel";
import {
  isPrivacyMode,
  isStreamMode,
  privacyCliFlags,
  type PrivacyMode,
  type StreamMode,
} from "../lib/hostPrefs";

export function HostModeView() {
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const start = async () => {
    setError(null);
    try {
      await invoke("start_host_agent", {});
      setStatus("Host agent start requested via daemon");
    } catch (e) {
      setError(String(e));
    }
  };

  const stop = async () => {
    setError(null);
    try {
      await invoke("stop_host_agent");
      setStatus("Host agent stop requested");
    } catch (e) {
      setError(String(e));
    }
  };

  const savePrivacy = async (mode: PrivacyMode) => {
    if (!isPrivacyMode(mode)) return;
    try {
      await invoke("set_setting", { key: "privacy_mode", value: mode });
      const flags = privacyCliFlags(mode);
      setStatus(
        `Privacy mode: ${flags.privacyMode} (enable on start: ${flags.enableOnSessionStart}). Applied on next Start host.`,
      );
    } catch (e) {
      setError(String(e));
    }
  };

  const saveStreamMode = async (mode: StreamMode) => {
    if (!isStreamMode(mode)) return;
    try {
      await invoke("set_setting", { key: "stream_mode", value: mode });
      setStatus(`Stream mode: ${mode} (applied on next Start host)`);
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="view">
      <header className="view__header">
        <div>
          <p className="eyebrow">Host</p>
          <h1>Host this machine</h1>
          <p className="subtitle">
            Starts <code>qubox-host-agent</code> through the daemon. Pairing
            approvals appear under Pairing. Use the tray for quick start/stop.
            File Sync drains the outbox over QUIC while a session is active.
          </p>
        </div>
      </header>
      {error ? <p className="state state--error">{error}</p> : null}
      {status ? <p className="state">{status}</p> : null}
      <div className="settings-grid">
        <div className="settings-field settings-field--inline">
          <button className="primary-button" onClick={() => void start()} type="button">
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>play_arrow</span>
            Start host
          </button>
          <button className="secondary-button" onClick={() => void stop()} type="button">
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>stop</span>
            Stop host
          </button>
        </div>
        <div className="settings-field">
          <span>Privacy mode (host)</span>
          <p className="subtitle">
            Applied when you Start host. Blank overlay is the portable fallback; vkms needs Linux setup.
          </p>
          <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
            <button className="secondary-button" onClick={() => void savePrivacy("none")} type="button">
              Off
            </button>
            <button
              className="secondary-button"
              onClick={() => void savePrivacy("blank-overlay")}
              type="button"
            >
              Blank overlay
            </button>
            <button className="secondary-button" onClick={() => void savePrivacy("vkms")} type="button">
              vkms (Linux)
            </button>
          </div>
        </div>
        <div className="settings-field">
          <span>Display streams</span>
          <p className="subtitle">Single stream (default) or multi-display capture on the host.</p>
          <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
            <button
              className="secondary-button"
              onClick={() => void saveStreamMode("single-stream")}
              type="button"
            >
              Single
            </button>
            <button
              className="secondary-button"
              onClick={() => void saveStreamMode("multi-display")}
              type="button"
            >
              Multi-display
            </button>
          </div>
        </div>
        <SharePanel />
      </div>
    </div>
  );
}
