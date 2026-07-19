import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useApp } from "./AppContext";

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
  signalingServer: "",
  bitrateKbps: 20_000,
  fpsCap: 60,
  decoderBackend: "ffmpeg",
  micEnabled: false,
  clipboardSync: "off",
  statsOverlay: true,
  autoStartHost: false,
};

const BITRATE_MIN = 1_000;
const BITRATE_MAX = 200_000;
const FPS_MIN = 15;
const FPS_MAX = 240;

export function SettingsView() {
  const { settings } = useApp();
  const [form, setForm] = useState<FormState>(DEFAULT_FORM);
  const [savingKey, setSavingKey] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saveOk, setSaveOk] = useState<string | null>(null);
  const [formErrors, setFormErrors] = useState<Record<string, string>>({});

  useEffect(() => {
    if (!settings) return;
    const decoder =
      settings.decoderBackend === "hw-decode" ? "hw-decode" : "ffmpeg";
    const clip = (settings.clipboardSync ??
      "off") as FormState["clipboardSync"];
    setForm({
      signalingServer: settings.signalingServer ?? "",
      bitrateKbps: settings.bitrateKbps ?? 20_000,
      fpsCap: settings.fpsCap ?? 60,
      decoderBackend: decoder,
      micEnabled: settings.micEnabled,
      clipboardSync: clip,
      statsOverlay: settings.statsOverlay,
      autoStartHost: settings.autoStartHost,
    });
  }, [settings]);

  const isHwDecode = form.decoderBackend === "hw-decode";

  const save = async (key: string, value: string, label: string) => {
    setSavingKey(key);
    setSaveError(null);
    setSaveOk(null);
    try {
      await invoke("set_setting", { key, value });
      setSaveOk(`Saved ${label}`);
    } catch (error) {
      setSaveError(String(error));
    } finally {
      setSavingKey(null);
    }
  };

  const boolToString = (b: boolean): string => (b ? "true" : "false");
  const numToString = (n: number): string => n.toString(10);

  const validateBitrate = (n: number): string | null => {
    if (!Number.isFinite(n)) return "Bitrate must be a number";
    if (n < BITRATE_MIN || n > BITRATE_MAX) {
      return `Bitrate must be between ${BITRATE_MIN} and ${BITRATE_MAX} kbps`;
    }
    return null;
  };

  const validateFps = (n: number): string | null => {
    if (!Number.isFinite(n)) return "FPS must be a number";
    if (n < FPS_MIN || n > FPS_MAX) {
      return `FPS cap must be between ${FPS_MIN} and ${FPS_MAX}`;
    }
    return null;
  };

  const validateSignaling = (raw: string): string | null => {
    const trimmed = raw.trim();
    if (trimmed.length === 0) return null;
    if (!/^wss?:\/\/.+/i.test(trimmed)) {
      return "Signaling URL must start with ws:// or wss://";
    }
    return null;
  };

  const summary = useMemo(() => {
    const errs: Record<string, string> = {};
    const sErr = validateSignaling(form.signalingServer);
    if (sErr) errs.signaling_server = sErr;
    const bErr = validateBitrate(form.bitrateKbps);
    if (bErr) errs.bitrate_kbps = bErr;
    const fErr = validateFps(form.fpsCap);
    if (fErr) errs.fps_cap = fErr;
    return errs;
  }, [form]);

  useEffect(() => {
    setFormErrors(summary);
  }, [summary]);

  return (
    <div className="view">
      <header className="view__header">
        <div>
          <p className="eyebrow">Settings</p>
          <h1>Client preferences</h1>
          <p className="subtitle">
            Changes are written through the daemon IPC. The signaling server and
            decoder backend are applied to the next launched session.
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
              setForm((prev) => ({
                ...prev,
                signalingServer: event.target.value,
              }))
            }
            type="text"
            value={form.signalingServer}
          />
          {formErrors.signaling_server ? (
            <p className="state state--error" style={{ fontSize: "0.8rem" }}>
              {formErrors.signaling_server}
            </p>
          ) : null}
          <button
            className="secondary-button"
            disabled={
              Boolean(formErrors.signaling_server) ||
              savingKey === "signaling_server"
            }
            onClick={() =>
              void save(
                "signaling_server",
                form.signalingServer.trim(),
                "signaling server",
              )
            }
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
              {savingKey === "signaling_server" ? "sync" : "save"}
            </span>
            {savingKey === "signaling_server" ? "Saving…" : "Save"}
          </button>
        </label>

        <label className="settings-field">
          <span>Bitrate cap (kbps)</span>
          <input
            className="text-input"
            max={BITRATE_MAX}
            min={BITRATE_MIN}
            onChange={(event) =>
              setForm((prev) => ({
                ...prev,
                bitrateKbps: Number(event.target.value),
              }))
            }
            step={1000}
            type="number"
            value={Number.isFinite(form.bitrateKbps) ? form.bitrateKbps : ""}
          />
          {formErrors.bitrate_kbps ? (
            <p className="state state--error" style={{ fontSize: "0.8rem" }}>
              {formErrors.bitrate_kbps}
            </p>
          ) : null}
          <button
            className="secondary-button"
            disabled={
              Boolean(formErrors.bitrate_kbps) || savingKey === "bitrate_kbps"
            }
            onClick={() =>
              void save(
                "bitrate_kbps",
                numToString(form.bitrateKbps),
                "bitrate cap",
              )
            }
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
              {savingKey === "bitrate_kbps" ? "sync" : "save"}
            </span>
            {savingKey === "bitrate_kbps" ? "Saving…" : "Save"}
          </button>
        </label>

        <label className="settings-field">
          <span>FPS cap</span>
          <input
            className="text-input"
            max={FPS_MAX}
            min={FPS_MIN}
            onChange={(event) =>
              setForm((prev) => ({
                ...prev,
                fpsCap: Number(event.target.value),
              }))
            }
            type="number"
            value={Number.isFinite(form.fpsCap) ? form.fpsCap : ""}
          />
          {formErrors.fps_cap ? (
            <p className="state state--error" style={{ fontSize: "0.8rem" }}>
              {formErrors.fps_cap}
            </p>
          ) : null}
          <button
            className="secondary-button"
            disabled={Boolean(formErrors.fps_cap) || savingKey === "fps_cap"}
            onClick={() =>
              void save("fps_cap", numToString(form.fpsCap), "FPS cap")
            }
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
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
              setForm((prev) => ({
                ...prev,
                decoderBackend:
                  event.target.value === "hw-decode" ? "hw-decode" : "ffmpeg",
              }))
            }
            value={form.decoderBackend}
          >
            <option value="ffmpeg">ffmpeg (subprocess)</option>
            <option value="hw-decode">HW decode (ffmpeg-next)</option>
          </select>
          {isHwDecode ? (
            <p className="state state--info" style={{ fontSize: "0.82rem" }}>
              Hardware decode requires working GPU drivers. If a session fails
              to start, switch back to <code>ffmpeg</code>.
            </p>
          ) : null}
          <button
            className="secondary-button"
            disabled={savingKey === "decoder_backend"}
            onClick={() =>
              void save(
                "decoder_backend",
                form.decoderBackend,
                "decoder backend",
              )
            }
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
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
            onClick={() =>
              void save(
                "mic_enabled",
                boolToString(form.micEnabled),
                "microphone forwarding",
              )
            }
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
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
              setForm((prev) => ({
                ...prev,
                clipboardSync: event.target.value as FormState["clipboardSync"],
              }))
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
            onClick={() =>
              void save("clipboard_sync", form.clipboardSync, "clipboard sync")
            }
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
              {savingKey === "clipboard_sync" ? "sync" : "save"}
            </span>
            {savingKey === "clipboard_sync" ? "Saving…" : "Save"}
          </button>
        </label>

        <label className="settings-field settings-field--inline">
          <input
            checked={form.statsOverlay}
            onChange={(event) =>
              setForm((prev) => ({
                ...prev,
                statsOverlay: event.target.checked,
              }))
            }
            type="checkbox"
          />
          <span>Show stats overlay in session</span>
          <button
            className="secondary-button"
            disabled={savingKey === "stats_overlay"}
            onClick={() =>
              void save(
                "stats_overlay",
                boolToString(form.statsOverlay),
                "stats overlay",
              )
            }
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
              {savingKey === "stats_overlay" ? "sync" : "save"}
            </span>
            {savingKey === "stats_overlay" ? "Saving…" : "Save"}
          </button>
        </label>

        <label className="settings-field settings-field--inline">
          <input
            checked={form.autoStartHost}
            onChange={(event) =>
              setForm((prev) => ({
                ...prev,
                autoStartHost: event.target.checked,
              }))
            }
            type="checkbox"
          />
          <span>Automatically start host on app open</span>
          <button
            className="secondary-button"
            disabled={savingKey === "auto_start_host"}
            onClick={() =>
              void save(
                "auto_start_host",
                boolToString(form.autoStartHost),
                "auto-start host",
              )
            }
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
              {savingKey === "auto_start_host" ? "sync" : "save"}
            </span>
            {savingKey === "auto_start_host" ? "Saving…" : "Save"}
          </button>
        </label>
      </div>
    </div>
  );
}
