import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  CLOUD_ACCOUNTS,
  CLOUD_SIGNALING,
  isValidEnrollCode,
  normalizeEnrollCode,
  resolveSignalingServer,
  type SetupMode,
} from "../lib/firstRun";
import { useApp } from "./AppContext";

type FirstRunProps = {
  onDone: () => void;
};

type Step = "mode" | "details";

export function FirstRun({ onDone }: FirstRunProps) {
  const { lanIp } = useApp();
  const [step, setStep] = useState<Step>("mode");
  const [mode, setMode] = useState<SetupMode>("cloud");
  const [name, setName] = useState("My laptop");
  const [enrollCode, setEnrollCode] = useState("");
  const [selfhostServer, setSelfhostServer] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);

  // Pre-fill self-host signaling URL with the detected LAN IP so users
  // don't accidentally copy `127.0.0.1` and break pairing for other
  // machines. Empty string falls back to "must be typed".
  useEffect(() => {
    if (lanIp) {
      setSelfhostServer((prev) => (prev ? prev : `ws://${lanIp}:7000/ws`));
    }
  }, [lanIp]);

  const pickMode = (m: SetupMode) => {
    setMode(m);
    setStep("details");
    setError(null);
  };

  const finish = async () => {
    const deviceName = name.trim();
    if (deviceName.length === 0) {
      setError("Device display name is required.");
      return;
    }
    const signaling = resolveSignalingServer(mode, selfhostServer);

    if (mode === "selfhost" && signaling === "") {
      setError("Self-host signaling server URL is required.");
      return;
    }
    if (mode === "cloud") {
      const code = normalizeEnrollCode(enrollCode);
      if (!isValidEnrollCode(code)) {
        setError(
          "Enter the enroll code from qubox.app → Dashboard (Generate enroll code).",
        );
        return;
      }
    }

    setBusy(true);
    setError(null);
    setStatus(null);
    try {
      if (mode === "cloud") {
        const code = normalizeEnrollCode(enrollCode);
        setStatus("Linking this device to Qubox Cloud…");
        await invoke("cloud_enroll", {
          code,
          displayName: deviceName,
          accountsUrl: CLOUD_ACCOUNTS,
        });
        setStatus("Saving preferences…");
      }

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
          ? `${msg} — start the daemon: qubox-daemon run`
          : msg,
      );
    } finally {
      setBusy(false);
      setStatus(null);
    }
  };

  if (step === "mode") {
    return (
      <div className="view first-run" data-testid="first-run">
        <header className="view__header">
          <div>
            <p className="eyebrow">Welcome</p>
            <h1 data-testid="first-run-title">How will you use Qubox?</h1>
            <p className="subtitle">
              Cloud uses Qubox accounts and{" "}
              <code>wss://signal.qubox.app</code>. Self-host talks to your own
              signaling server (LAN or private VPS).
            </p>
          </div>
        </header>

        <div className="mode-cards">
          <button
            type="button"
            className="mode-card mode-card--primary"
            data-testid="mode-cloud"
            onClick={() => pickMode("cloud")}
          >
            <span className="mode-card__badge">Recommended</span>
            <h2>Qubox Cloud</h2>
            <p>
              Sign in at qubox.app, generate an enroll code, link this machine.
              TLS signaling + TURN for hard NAT.
            </p>
            <ul>
              <li>Account + device list on the web</li>
              <li>One-time enroll code (15 min)</li>
              <li>Tenant isolation on the relay</li>
            </ul>
          </button>

          <button
            type="button"
            className="mode-card"
            data-testid="mode-selfhost"
            onClick={() => pickMode("selfhost")}
          >
            <h2>Self-host</h2>
            <p>
              Point at your own signaling URL. No cloud account required —
              pairing stays local / on your server.
            </p>
            <ul>
              <li>ws:// or wss:// to your host</li>
              <li>Works offline / air-gapped</li>
              <li>You run coturn if needed</li>
            </ul>
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="view first-run" data-testid="first-run-details">
      <header className="view__header">
        <div>
          <p className="eyebrow">
            {mode === "cloud" ? "Qubox Cloud" : "Self-host"}
          </p>
          <h1 data-testid="first-run-details-title">Finish setup</h1>
          <p className="subtitle">
            {mode === "cloud" ? (
              <>
                On{" "}
                <a
                  href="https://qubox.app/app"
                  target="_blank"
                  rel="noreferrer"
                >
                  qubox.app/app
                </a>
                , click <strong>Generate enroll code</strong>, then paste it
                below.
              </>
            ) : (
              <>
                Enter the WebSocket URL of your signaling server. Leave blank
                to type it after setup.
              </>
            )}
          </p>
        </div>
        <button
          type="button"
          className="ghost-button"
          onClick={() => {
            setStep("mode");
            setError(null);
          }}
        >
          ← Change mode
        </button>
      </header>

      {error ? (
        <p className="state state--error" data-testid="first-run-error">
          {error}
        </p>
      ) : null}
      {status ? <p className="state state--info">{status}</p> : null}

      <div className="settings-grid">
        <label className="settings-field">
          <span>Device display name</span>
          <input
            className="text-input"
            data-testid="device-name"
            onChange={(e) => setName(e.target.value)}
            value={name}
            autoComplete="off"
          />
        </label>

        {mode === "cloud" ? (
          <label className="settings-field">
            <span>Enroll code</span>
            <input
              className="text-input text-input--code"
              data-testid="enroll-code"
              onChange={(e) => setEnrollCode(e.target.value)}
              placeholder="e.g. KXRETMZS"
              value={enrollCode}
              autoComplete="off"
              spellCheck={false}
            />
            <p className="subtitle">
              Signaling: <code>{CLOUD_SIGNALING}</code>
            </p>
          </label>
        ) : (
          <label className="settings-field">
            <span>Signaling server</span>
            <input
              className="text-input"
              onChange={(e) => setSelfhostServer(e.target.value)}
              placeholder={lanIp ? `ws://${lanIp}:7000/ws` : "ws://your-host:7000/ws"}
              value={selfhostServer}
            />
            <p className="subtitle">
              {lanIp ? (
                <>Auto-detected LAN IP <code>{lanIp}</code>. </>)
              : null}
              Example: <code>ws://192.168.1.10:7000/ws</code> or{" "}
              <code>wss://signal.home.lan/ws</code>. Do not use{" "}
              <code>127.0.0.1</code> for other machines.
            </p>
          </label>
        )}

        <div className="settings-field">
          <span>After continue</span>
          <ul className="subtitle">
            <li>Daemon stores name + signaling URL atomically</li>
            {mode === "cloud" ? (
              <li>This device appears under your account</li>
            ) : (
              <li>Pair hosts on your private network</li>
            )}
            <li>Use Hosts / tray to stream</li>
          </ul>
        </div>

        <button
          className="primary-button"
          data-testid="first-run-continue"
          disabled={busy}
          onClick={() => void finish()}
          type="button"
        >
          {busy
            ? mode === "cloud"
              ? "Linking…"
              : "Saving…"
            : mode === "cloud"
              ? "Link device & continue"
              : "Continue"}
        </button>
      </div>
    </div>
  );
}
