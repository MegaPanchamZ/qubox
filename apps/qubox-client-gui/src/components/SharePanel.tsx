import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { shareCodeSvg, shareLinkPayload } from "../lib/qr";

type ShareLinkPayload = {
  code: string;
  urlHint: string;
  expiresUnixMs: number;
};

export function SharePanel() {
  const [code, setCode] = useState<string | null>(null);
  const [url, setUrl] = useState<string | null>(null);
  const [expiresMs, setExpiresMs] = useState<number | null>(null);
  const [redeem, setRedeem] = useState("");
  const [msg, setMsg] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const create = async () => {
    setErr(null);
    try {
      const res = await invoke<ShareLinkPayload>("create_share_link", { ttlSecs: 900 });
      setCode(res.code);
      setUrl(res.urlHint || null);
      setExpiresMs(res.expiresUnixMs || null);
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

  const qrSvg = code ? shareCodeSvg(code) : null;

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
          {url ? (
            <p className="subtitle">
              <a href={shareLinkPayload(code, url)} target="_blank" rel="noreferrer">
                {shareLinkPayload(code, url)}
              </a>
            </p>
          ) : null}
          {expiresMs ? (
            <p className="subtitle">
              expires {new Date(expiresMs).toLocaleTimeString()}
            </p>
          ) : null}
          {qrSvg ? (
            <div
              className="share-qr"
              aria-label="Share code QR"
              role="img"
              style={{ maxWidth: 220, marginTop: 8 }}
              dangerouslySetInnerHTML={{ __html: qrSvg }}
            />
          ) : null}
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
