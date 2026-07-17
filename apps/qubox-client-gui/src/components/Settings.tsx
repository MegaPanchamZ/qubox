import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useApp } from "./AppContext";
import type { Settings } from "./AppContext";

type FormState = {
  signalingServer: string;
  bitrateKbps: string;
  fpsCap: string;
  decoderBackend: string;
  micEnabled: boolean;
  clipboardSync: string;
  statsOverlay: boolean;
};

const DEFAULT_FORM: FormState = {
  signalingServer: "",
  bitrateKbps: "20000",
  fpsCap: "60",
  decoderBackend: "ffmpeg",
  micEnabled: false,
  clipboardSync: "off",
  statsOverlay: true,
};

export function SettingsView() {
  const { settings, setSettings } = useApp();
  const [form, setForm] = useState<FormState>(DEFAULT_FORM);
  const [savingKey, setSavingKey] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saveOk, setSaveOk] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    const load = async () => {
      try {
        const loaded = await invoke<Settings>("get_settings");
        if (cancelled) {
          return;
        }
        setSettings(loaded);
        setForm({
          signalingServer: loaded.signalingServer ?? "",
          bitrateKbps: String(loaded.bitrateKbps ?? 20000),
          fpsCap: String(loaded.fpsCap ?? 60),
          decoderBackend: loaded.decoderBackend ?? "ffmpeg",
          micEnabled: loaded.micEnabled,
          clipboardSync: loaded.clipboardSync ?? "off",
          statsOverlay: loaded.statsOverlay,
        });
      } catch (error) {
        if (cancelled) {
          return;
        }
        setSaveError(String(error));
      }
    };
    void load();
    return () => {
      cancelled = true;
    };
  }, [setSettings]);

  const save = async (key: string, value: string) => {
    setSavingKey(key);
    setSaveError(null);
    setSaveOk(null);
    try {
      await invoke("set_setting", { key, value });
      setSaveOk(`Saved ${key}`);
    } catch (error) {
      setSaveError(String(error));
    } finally {
      setSavingKey(null);
    }
  };

  return (
    <div className="view">
      <header className="view__header">
        <div>
          <p className="eyebrow">Settings</p>
          <h1>Client preferences</h1>
          <p className="subtitle">
            Changes are written through the daemon IPC. The signaling server
            and decoder backend are applied to the next launched session.
          </p>
        </div>
      </header>

      {saveError ? <p className="state state--error">{saveError}</p> : null}
      {saveOk ? <p className="state">{saveOk}</p> : null}
      {!settings ? <p className="state">Loading settings…</p> : null}

      <div className="settings-grid">
        <label className="settings-field">
          <span>Signaling server</span>
          <input
            className="text-input"
            onChange={(event) =>
              setForm((prev) => ({ ...prev, signalingServer: event.target.value }))
            }
            type="text"
            value={form.signalingServer}
          />
          <button
            className="secondary-button"
            disabled={savingKey === "signaling_server"}
            onClick={() => void save("signaling_server", form.signalingServer)}
            type="button"
          >
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>
              {savingKey === "signaling_server" ? "sync" : "save"}
            </span>
            {savingKey === "signaling_server" ? "Saving…" : "Save"}
          </button>
        </label>

        <label className="settings-field">
          <span>Bitrate cap (kbps)</span>
          <input
            className="text-input"
            min="1000"
            onChange={(event) =>
              setForm((prev) => ({ ...prev, bitrateKbps: event.target.value }))
            }
            step="1000"
            type="number"
            value={form.bitrateKbps}
          />
          <button
            className="secondary-button"
            disabled={savingKey === "bitrate_kbps"}
            onClick={() => void save("bitrate_kbps", form.bitrateKbps)}
            type="button"
          >
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>
              {savingKey === "bitrate_kbps" ? "sync" : "save"}
            </span>
            {savingKey === "bitrate_kbps" ? "Saving…" : "Save"}
          </button>
        </label>

        <label className="settings-field">
          <span>FPS cap</span>
          <input
            className="text-input"
            max="240"
            min="15"
            onChange={(event) =>
              setForm((prev) => ({ ...prev, fpsCap: event.target.value }))
            }
            type="number"
            value={form.fpsCap}
          />
          <button
            className="secondary-button"
            disabled={savingKey === "fps_cap"}
            onClick={() => void save("fps_cap", form.fpsCap)}
            type="button"
          >
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>
              {savingKey === "fps_cap" ? "sync" : "save"}
            </span>
            {savingKey === "fps_cap" ? "Saving…" : "Save"}
          </button>
        </label>

        <label className="settings-field">
          <span>Decoder backend</span>
          <select
            className="text-input"
            onChange={(event) =>
              setForm((prev) => ({ ...prev, decoderBackend: event.target.value }))
            }
            value={form.decoderBackend}
          >
            <option value="ffmpeg">ffmpeg (subprocess)</option>
            <option value="hw-decode">HW decode (ffmpeg-next)</option>
          </select>
          <button
            className="secondary-button"
            disabled={savingKey === "decoder_backend"}
            onClick={() => void save("decoder_backend", form.decoderBackend)}
            type="button"
          >
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>
              {savingKey === "decoder_backend" ? "sync" : "save"}
            </span>
            {savingKey === "decoder_backend" ? "Saving…" : "Save"}
          </button>
        </label>

        <label className="settings-field settings-field--inline">
          <input
            checked={form.micEnabled}
            onChange={(event) =>
              setForm((prev) => ({ ...prev, micEnabled: event.target.checked }))
            }
            type="checkbox"
          />
          <span>Enable microphone forwarding by default</span>
          <button
            className="secondary-button"
            disabled={savingKey === "mic_enabled"}
            onClick={() => void save("mic_enabled", String(form.micEnabled))}
            type="button"
          >
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>
              {savingKey === "mic_enabled" ? "sync" : "save"}
            </span>
            {savingKey === "mic_enabled" ? "Saving…" : "Save"}
          </button>
        </label>

        <label className="settings-field">
          <span>Clipboard sync</span>
          <select
            className="text-input"
            onChange={(event) =>
              setForm((prev) => ({ ...prev, clipboardSync: event.target.value }))
            }
            value={form.clipboardSync}
          >
            <option value="off">Off</option>
            <option value="host_to_client">Host → Client</option>
            <option value="client_to_host">Client → Host</option>
            <option value="both">Both directions</option>
          </select>
          <button
            className="secondary-button"
            disabled={savingKey === "clipboard_sync"}
            onClick={() => void save("clipboard_sync", form.clipboardSync)}
            type="button"
          >
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>
              {savingKey === "clipboard_sync" ? "sync" : "save"}
            </span>
            {savingKey === "clipboard_sync" ? "Saving…" : "Save"}
          </button>
        </label>

        <label className="settings-field settings-field--inline">
          <input
            checked={form.statsOverlay}
            onChange={(event) =>
              setForm((prev) => ({ ...prev, statsOverlay: event.target.checked }))
            }
            type="checkbox"
          />
          <span>Show stats overlay in session</span>
          <button
            className="secondary-button"
            disabled={savingKey === "stats_overlay"}
            onClick={() => void save("stats_overlay", String(form.statsOverlay))}
            type="button"
          >
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>
              {savingKey === "stats_overlay" ? "sync" : "save"}
            </span>
            {savingKey === "stats_overlay" ? "Saving…" : "Save"}
          </button>
        </label>
      </div>
    </div>
  );
}
