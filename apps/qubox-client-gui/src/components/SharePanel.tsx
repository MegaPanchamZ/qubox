import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/** Text share code + simple QR-like block (code string). Full QR lib optional. */
export function SharePanel() {
  const [code, setCode] = useState<string | null>(null);
  const [url, setUrl] = useState<string | null>(null);
  const [redeem, setRedeem] = useState("");
  const [msg, setMsg] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const create = async () => {
    setErr(null);
    try {
      const raw = await invoke<string>("create_share_link", { ttlSecs: 900 });
      // CLI prints share code=… url=…
      const codeM = /code=(\S+)/.exec(raw) ?? /"code"\s*:\s*"([^"]+)"/.exec(raw);
      const urlM = /url=(\S+)/.exec(raw) ?? /"url_hint"\s*:\s*"([^"]+)"/.exec(raw);
      setCode(codeM?.[1] ?? raw);
      setUrl(urlM?.[1] ?? null);
      setMsg("Share link created (15 min)");
    } catch (e) {
      setErr(String(e));
    }
  };

  const doRedeem = async () => {
    setErr(null);
    try {
      await invoke("redeem_share_link", { code: redeem.trim() });
      setMsg("Redeem requested — wait for host approval if required");
    } catch (e) {
      setErr(String(e));
    }
  };

  return (
    <div className="settings-field">
      <span>Share link</span>
      <p className="subtitle">Guest joins with a short code (no account required).</p>
      {err ? <p className="state state--error">{err}</p> : null}
      {msg ? <p className="state">{msg}</p> : null}
      <button className="secondary-button" onClick={() => void create()} type="button">
        Create share link
      </button>
      {code ? (
        <div className="share-code-box">
          <code className="share-code">{code}</code>
          {url ? <p className="subtitle">{url}</p> : null}
          <pre className="share-qr-fallback" aria-label="Share code">
            {`┌────────────┐\n│  ${code.slice(0, 8).padEnd(8)}  │\n│  QUBOX     │\n└────────────┘`}
          </pre>
          <button
            className="secondary-button"
            onClick={() => void navigator.clipboard.writeText(code)}
            type="button"
          >
            Copy code
          </button>
        </div>
      ) : null}
      <label className="settings-field">
        <span>Redeem code</span>
        <input
          className="text-input"
          onChange={(e) => setRedeem(e.target.value)}
          placeholder="paste code"
          value={redeem}
        />
        <button className="secondary-button" onClick={() => void doRedeem()} type="button">
          Redeem
        </button>
      </label>
    </div>
  );
}
