import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useApp } from "./AppContext";
import { SharePanel } from "./SharePanel";
import {
  isPrivacyMode,
  isStreamMode,
  normalizePrivacyMode,
  normalizeStreamMode,
  privacyCliFlags,
  type PrivacyMode,
  type StreamMode,
} from "../lib/hostPrefs";

const STREAM_MODES: { id: StreamMode; label: string }[] = [
  { id: "single-stream", label: "Single" },
  { id: "multi-display", label: "Multi-display" },
  { id: "all-displays", label: "All displays" },
];

export function HostModeView() {
  const { hostRunning, activeSessions } = useApp();
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [stopArmed, setStopArmed] = useState(false);
  const isRunning = hostRunning === true;

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
    if (activeSessions.length > 0 && !stopArmed) {
      setStopArmed(true);
      setError(
        `${activeSessions.length} active session${activeSessions.length === 1 ? "" : "s"} connected. ` +
          "Stopping the host will disconnect them. Click again to confirm.",
      );
      return;
    }
    setError(null);
    try {
      await invoke("stop_host_agent");
      setStatus("Host agent stop requested");
      setStopArmed(false);
    } catch (e) {
      setError(String(e));
    }
  };

  const savePrivacy = async (mode: PrivacyMode) => {
    if (!isPrivacyMode(mode)) return;
    try {
      await invoke("set_setting", { key: "privacy_mode", value: mode });
      const flags = privacyCliFlags(mode);
      const note = isRunning ? " (applies on next Start host)" : "";
      setStatus(
        `Privacy mode: ${flags.privacyMode} (enable on start: ${flags.enableOnSessionStart})${note}.`,
      );
      if (isRunning) {
        setError(
          "Privacy changes only take effect when the host agent is restarted.",
        );
      }
    } catch (e) {
      setError(String(e));
    }
  };

  const saveStreamMode = async (mode: StreamMode) => {
    if (!isStreamMode(mode)) return;
    try {
      await invoke("set_setting", { key: "stream_mode", value: mode });
      setStatus(
        `Stream mode: ${mode}${isRunning ? " (applies on next Start host)" : ""}`,
      );
      if (isRunning) {
        setError(
          "Stream mode changes only take effect when the host agent is restarted.",
        );
      }
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
          {isRunning ? (
            <button
              className="secondary-button"
              onClick={() => void stop()}
              type="button"
              style={{
                borderColor: stopArmed
                  ? "var(--color-error)"
                  : "var(--color-error)",
                color: "var(--color-error)",
              }}
            >
              <span
                className="material-symbols-outlined"
                style={{ fontSize: "1.1rem" }}
              >
                stop
              </span>
              {stopArmed ? "Confirm stop host" : "Stop host"}
            </button>
          ) : (
            <button
              className="primary-button"
              onClick={() => void start()}
              type="button"
            >
              <span
                className="material-symbols-outlined"
                style={{ fontSize: "1.1rem" }}
              >
                play_arrow
              </span>
              Start host
            </button>
          )}
        </div>
        <div className="settings-field">
          <span>Privacy mode (host)</span>
          <p className="subtitle">
            Applied when you Start host. Blank overlay is the portable fallback;
            vkms needs Linux setup.
          </p>
          <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
            <button
              className="secondary-button"
              onClick={() => void savePrivacy("none")}
              type="button"
            >
              Off
            </button>
            <button
              className="secondary-button"
              onClick={() => void savePrivacy("blank-overlay")}
              type="button"
            >
              Blank overlay
            </button>
            <button
              className="secondary-button"
              onClick={() => void savePrivacy("vkms")}
              type="button"
            >
              vkms (Linux)
            </button>
          </div>
        </div>
        <div className="settings-field">
          <span>Display streams</span>
          <p className="subtitle">
            Single stream, multi-display, or all-display capture on the host.
          </p>
          <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
            {STREAM_MODES.map((m) => (
              <button
                key={m.id}
                className="secondary-button"
                onClick={() => void saveStreamMode(m.id)}
                type="button"
              >
                {m.label}
              </button>
            ))}
          </div>
        </div>
        <SharePanel />
      </div>
    </div>
  );
}

/* Re-export the normalizers for tests that consume this module. */
export { normalizePrivacyMode, normalizeStreamMode };
