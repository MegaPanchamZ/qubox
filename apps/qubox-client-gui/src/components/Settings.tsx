import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useApp } from "./AppContext";
import { FileSyncView } from "./FileSync";
import { DEFAULT_RELAY_SERVER } from "./FirstRun";

type FormState = {
  signalingServer: string;
  bitrateKbps: number;
  fpsCap: number;
  decoderBackend: "ffmpeg" | "hw-decode";
  micEnabled: boolean;
  clipboardSync: "off" | "host_to_client" | "client_to_host" | "both";
  statsOverlay: boolean;
  autoStartHost: boolean;
};

const DEFAULT_FORM: FormState = {
  signalingServer: DEFAULT_RELAY_SERVER,
  bitrateKbps: 20_000,
  fpsCap: 60,
  decoderBackend: "ffmpeg",
  micEnabled: false,
  clipboardSync: "both",
  statsOverlay: true,
  autoStartHost: false,
};

export function SettingsView() {
  const { settings, refreshSettings } = useApp();
  const [form, setForm] = useState<FormState>(DEFAULT_FORM);
  const [saveOk, setSaveOk] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  useEffect(() => {
    if (!settings) return;
    const decoder =
      settings.decoderBackend === "hw-decode" ? "hw-decode" : "ffmpeg";
    const clip = (settings.clipboardSync ??
      "off") as FormState["clipboardSync"];
    setForm({
      signalingServer: settings.signalingServer || DEFAULT_RELAY_SERVER,
      bitrateKbps: settings.bitrateKbps ?? 20_000,
      fpsCap: settings.fpsCap ?? 60,
      decoderBackend: decoder,
      micEnabled: settings.micEnabled,
      clipboardSync: clip,
      statsOverlay: settings.statsOverlay,
      autoStartHost: settings.autoStartHost,
    });
  }, [settings]);

  const saveKey = async (key: string, value: string, label: string) => {
    setSaveError(null);
    try {
      await invoke("set_setting", { key, value });
      await refreshSettings();
      setSaveOk(`Saved ${label}`);
    } catch (error) {
      setSaveError(String(error));
    }
  };

  return (
    <div className="view" data-testid="settings-view">
      <header className="view__header">
        <div>
          <p className="eyebrow">Preferences</p>
          <h1>Settings</h1>
          <p className="subtitle">
            Customize video quality, audio, clipboard sharing, and server
            options.
          </p>
        </div>
      </header>

      {saveError ? <p className="state state--error">{saveError}</p> : null}
      {saveOk ? <p className="state state--info">{saveOk}</p> : null}

      <section className="section">
        <h2 className="section__title">General &amp; Sharing</h2>
        <div className="settings-grid">
          <label className="settings-field settings-field--inline">
            <input
              checked={form.autoStartHost}
              onChange={(e) => {
                const next = e.target.checked;
                setForm((p) => ({ ...p, autoStartHost: next }));
                void saveKey(
                  "auto_start_host",
                  next ? "true" : "false",
                  "auto-start sharing",
                );
              }}
              type="checkbox"
            />
            <span>Automatically allow remote connections when Qubox opens</span>
          </label>

          <label className="settings-field settings-field--inline">
            <input
              checked={form.micEnabled}
              onChange={(e) => {
                const next = e.target.checked;
                setForm((p) => ({ ...p, micEnabled: next }));
                void saveKey(
                  "mic_enabled",
                  next ? "true" : "false",
                  "microphone sharing",
                );
              }}
              type="checkbox"
            />
            <span>Forward microphone audio during remote sessions</span>
          </label>

          <label className="settings-field">
            <span>Clipboard Sync</span>
            <select
              className="text-input"
              onChange={(e) => {
                const next = e.target.value as FormState["clipboardSync"];
                setForm((p) => ({ ...p, clipboardSync: next }));
                void saveKey("clipboard_sync", next, "clipboard sync");
              }}
              value={form.clipboardSync}
            >
              <option value="both">Both directions (Copy &amp; Paste anywhere)</option>
              <option value="host_to_client">Host to Client only</option>
              <option value="client_to_host">Client to Host only</option>
              <option value="off">Off</option>
            </select>
          </label>

          <label className="settings-field settings-field--inline">
            <input
              checked={form.statsOverlay}
              onChange={(e) => {
                const next = e.target.checked;
                setForm((p) => ({ ...p, statsOverlay: next }));
                void saveKey(
                  "stats_overlay",
                  next ? "true" : "false",
                  "performance overlay",
                );
              }}
              type="checkbox"
            />
            <span>Show FPS &amp; latency overlay during sessions</span>
          </label>
        </div>
      </section>

      <section className="section">
        <h2 className="section__title">Stream Quality</h2>
        <div className="settings-grid">
          <label className="settings-field">
            <span>Frame Rate (FPS)</span>
            <select
              className="text-input"
              onChange={(e) => {
                const next = Number(e.target.value);
                setForm((p) => ({ ...p, fpsCap: next }));
                void saveKey("fps_cap", String(next), "frame rate");
              }}
              value={form.fpsCap}
            >
              <option value={30}>30 FPS (Battery saver)</option>
              <option value={60}>60 FPS (Standard smooth)</option>
              <option value={120}>120 FPS (High refresh)</option>
              <option value={144}>144 FPS (Ultra low latency)</option>
            </select>
          </label>

          <label className="settings-field">
            <span>Max Video Bitrate</span>
            <select
              className="text-input"
              onChange={(e) => {
                const next = Number(e.target.value);
                setForm((p) => ({ ...p, bitrateKbps: next }));
                void saveKey("bitrate_kbps", String(next), "video bitrate");
              }}
              value={form.bitrateKbps}
            >
              <option value={5_000}>5 Mbps (Low bandwidth)</option>
              <option value={15_000}>15 Mbps (Balanced 1080p)</option>
              <option value={20_000}>20 Mbps (High quality)</option>
              <option value={50_000}>50 Mbps (4K / LAN)</option>
            </select>
          </label>

          <label className="settings-field">
            <span>Video Decoder</span>
            <select
              className="text-input"
              onChange={(e) => {
                const next =
                  e.target.value === "hw-decode" ? "hw-decode" : "ffmpeg";
                setForm((p) => ({ ...p, decoderBackend: next }));
                void saveKey("decoder_backend", next, "video decoder");
              }}
              value={form.decoderBackend}
            >
              <option value="ffmpeg">Standard (Compatible with all GPUs)</option>
              <option value="hw-decode">Hardware Accelerated (GPU)</option>
            </select>
          </label>
        </div>
      </section>

      <details className="host-card" style={{ cursor: "pointer" }}>
        <summary style={{ fontWeight: 700, fontSize: "1rem" }}>
          Advanced Server &amp; File Sync Options
        </summary>
        <div
          style={{
            marginTop: 20,
            display: "flex",
            flexDirection: "column",
            gap: 20,
            cursor: "default",
          }}
        >
          <label className="settings-field">
            <span>Server URL</span>
            <div style={{ display: "flex", gap: 10, flexWrap: "wrap" }}>
              <input
                className="text-input"
                onChange={(e) =>
                  setForm((p) => ({ ...p, signalingServer: e.target.value }))
                }
                style={{ flex: 1, minWidth: 260 }}
                type="text"
                value={form.signalingServer}
              />
              <button
                className="secondary-button"
                onClick={() =>
                  void saveKey(
                    "signaling_server",
                    form.signalingServer.trim(),
                    "server URL",
                  )
                }
                type="button"
              >
                Save Server URL
              </button>
            </div>
          </label>

          <FileSyncView />
        </div>
      </details>
    </div>
  );
}
