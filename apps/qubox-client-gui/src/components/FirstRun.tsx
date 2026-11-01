import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  CLOUD_ACCOUNTS,
  isValidEnrollCode,
  normalizeEnrollCode,
  resolveSignalingServer,
  type SetupMode,
} from "../lib/firstRun";

export const DEFAULT_RELAY_SERVER = "ws://170.64.201.135:7000/ws";

type FirstRunProps = {
  onDone: () => void;
};

export function FirstRun({ onDone }: FirstRunProps) {
  const [name, setName] = useState("My Computer");
  const [mode, setMode] = useState<SetupMode>("selfhost");
  const [customServer, setCustomServer] = useState(DEFAULT_RELAY_SERVER);
  const [enrollCode, setEnrollCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);

  const finish = async () => {
    const deviceName = name.trim();
    if (deviceName.length === 0) {
      setError("Please enter a name for this computer.");
      return;
    }
    const signaling =
      mode === "cloud"
        ? resolveSignalingServer("cloud", "")
        : customServer.trim() || DEFAULT_RELAY_SERVER;

    if (mode === "cloud") {
      const code = normalizeEnrollCode(enrollCode);
      if (!isValidEnrollCode(code)) {
        setError("Enter a valid enroll code from qubox.app.");
        return;
      }
    }

    setBusy(true);
    setError(null);
    setStatus(null);
    try {
      if (mode === "cloud") {
        const code = normalizeEnrollCode(enrollCode);
        setStatus("Linking this computer…");
        await invoke("cloud_enroll", {
          code,
          displayName: deviceName,
          accountsUrl: CLOUD_ACCOUNTS,
        });
      }

      setStatus("Saving setup…");
      await invoke("complete_onboarding", {
        deviceName,
        signalingServer: signaling,
        cloudMode: mode === "cloud",
        accountsUrl: mode === "cloud" ? CLOUD_ACCOUNTS : null,
      });
      onDone();
    } catch (e) {
      const msg = String(e);
      setError(
        msg.includes("daemon") || msg.includes("connect")
          ? `${msg} — make sure qubox-daemon is running`
          : msg,
      );
    } finally {
      setBusy(false);
      setStatus(null);
    }
  };

  return (
    <div
      className="view first-run"
      data-testid="first-run"
      style={{ maxWidth: 560, margin: "40px auto" }}
    >
      <header className="view__header" style={{ borderBottom: "none", paddingBottom: 0 }}>
        <div>
          <p className="eyebrow">Welcome to Qubox</p>
          <h1 data-testid="first-run-title" style={{ fontSize: "2.2rem" }}>
            Name this computer
          </h1>
          <p className="subtitle">
            Give this computer a friendly name so you can recognize it when
            connecting from your other devices.
          </p>
        </div>
      </header>

      {error ? (
        <p className="state state--error" data-testid="first-run-error">
          {error}
        </p>
      ) : null}
      {status ? <p className="state state--info">{status}</p> : null}

      <div className="host-card" style={{ gap: 20 }}>
        <label className="settings-field" style={{ padding: 0, border: "none", background: "transparent", boxShadow: "none" }}>
          <span style={{ fontWeight: 600, fontSize: "0.95rem" }}>
            Computer Name
          </span>
          <input
            className="text-input"
            data-testid="device-name"
            onChange={(e) => setName(e.target.value)}
            placeholder="e.g. Work Laptop or Gaming PC"
            value={name}
            autoComplete="off"
          />
        </label>

        <details style={{ color: "var(--muted)", fontSize: "0.9rem" }}>
          <summary style={{ cursor: "pointer", userSelect: "none" }}>
            Advanced server options (optional)
          </summary>
          <div style={{ marginTop: 14, display: "flex", flexDirection: "column", gap: 12 }}>
            <div style={{ display: "flex", gap: 8 }}>
              <button
                type="button"
                className={mode === "selfhost" ? "primary-button" : "secondary-button"}
                data-testid="mode-selfhost"
                onClick={() => setMode("selfhost")}
              >
                Direct / Relay Server
              </button>
              <button
                type="button"
                className={mode === "cloud" ? "primary-button" : "secondary-button"}
                data-testid="mode-cloud"
                onClick={() => setMode("cloud")}
              >
                Qubox Cloud Code
              </button>
            </div>

            {mode === "selfhost" ? (
              <label style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                <span>Signaling &amp; Relay Server URL</span>
                <input
                  className="text-input"
                  onChange={(e) => setCustomServer(e.target.value)}
                  placeholder={DEFAULT_RELAY_SERVER}
                  value={customServer}
                />
              </label>
            ) : (
              <label style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                <span>Cloud Enroll Code</span>
                <input
                  className="text-input text-input--code"
                  data-testid="enroll-code"
                  onChange={(e) => setEnrollCode(e.target.value)}
                  placeholder="e.g. KXRETMZS"
                  value={enrollCode}
                  autoComplete="off"
                  spellCheck={false}
                />
              </label>
            )}
          </div>
        </details>

        <button
          className="primary-button"
          data-testid="first-run-continue"
          disabled={busy}
          onClick={() => void finish()}
          style={{ width: "100%", padding: "0.95rem 1.5rem", fontSize: "0.9rem" }}
          type="button"
        >
          {busy ? "Setting up…" : "Get Started →"}
        </button>
      </div>
    </div>
  );
}
