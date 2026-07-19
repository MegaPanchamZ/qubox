import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useApp } from "./AppContext";
import type { PairingRequest } from "./AppContext";
import { SharePanel } from "./SharePanel";

type HostPending = {
  request_id: string;
  client_peer_id: string;
  client_device_id: string;
  client_name: string;
  client_label: string;
  received_at_unix_ms: number;
};

export function PairingRequests() {
  const { pendingPairings, removePairingRequest, settings } = useApp();
  const [hostPending, setHostPending] = useState<HostPending[]>([]);
  const [hostErr, setHostErr] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const cloud = settings?.cloudMode ?? false;

  const refreshHost = useCallback(async () => {
    try {
      const list = await invoke<HostPending[]>("list_host_pending_pairings");
      setHostPending(Array.isArray(list) ? list : []);
      setHostErr(null);
    } catch (e) {
      setHostPending([]);
      setHostErr(String(e));
    }
  }, []);

  useEffect(() => {
    void refreshHost();
    const t = setInterval(() => void refreshHost(), 2000);
    return () => clearInterval(t);
  }, [refreshHost]);

  const acceptClient = async (request: PairingRequest) => {
    setBusy(request.requestId);
    try {
      await invoke("accept_pairing", { hostId: request.hostId });
      setToast("Local pairing grant saved");
    } catch (error) {
      setToast(`Approve failed: ${error}`);
    } finally {
      removePairingRequest(request.requestId);
      setBusy(null);
    }
  };

  const rejectClient = async (request: PairingRequest) => {
    setBusy(request.requestId);
    try {
      await invoke("reject_pairing", { hostId: request.hostId });
    } catch (error) {
      setToast(`Reject failed: ${error}`);
    } finally {
      removePairingRequest(request.requestId);
      setBusy(null);
    }
  };

  const decideHost = async (requestId: string, approved: boolean) => {
    setBusy(requestId);
    try {
      await invoke("host_pairing_decide", { requestId, approved });
      setToast(approved ? "Client approved — they can start a session" : "Request rejected");
      await refreshHost();
    } catch (e) {
      setToast(`Decision failed: ${e}`);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="view">
      <header className="view__header">
        <div>
          <p className="eyebrow">Pairing</p>
          <h1>Pairing &amp; share</h1>
          <p className="subtitle">
            {cloud ? (
              <>
                Cloud mode: clients enroll on qubox.app, then request pairing
                with your host. Approve requests below while{" "}
                <strong>host agent</strong> is running.
              </>
            ) : (
              <>
                Self-host: approve pairing on this machine, or share a short
                code for guests.
              </>
            )}
          </p>
        </div>
      </header>

      {toast ? <p className="state">{toast}</p> : null}

      <section className="settings-field" style={{ marginBottom: 20 }}>
        <span>As host — approve clients</span>
        <p className="subtitle">
          When someone pairs with this PC, requests appear here (host-agent
          must be running).
        </p>
        {hostErr ? (
          <p className="state state--error" style={{ fontSize: "0.85rem" }}>
            {hostErr.includes("unreachable")
              ? "Host agent not running — start hosting from Host mode or tray."
              : hostErr}
          </p>
        ) : null}
        {hostPending.length === 0 ? (
          <p className="empty-state__body">No client requests right now.</p>
        ) : (
          <div className="pairing-grid">
            {hostPending.map((r) => (
              <article className="pairing-card" key={r.request_id}>
                <p className="host-card__label">Wants to pair</p>
                <h2>{r.client_name || r.client_label || "Client"}</h2>
                {r.client_label && r.client_label !== r.client_name ? (
                  <p className="host-card__meta">Label: {r.client_label}</p>
                ) : null}
                <p className="host-card__id">
                  peer {r.client_peer_id.slice(0, 8)}… · device{" "}
                  {r.client_device_id.slice(0, 8)}…
                </p>
                <p className="host-card__meta">
                  {new Date(r.received_at_unix_ms).toLocaleTimeString()}
                </p>
                <div className="pairing-card__actions">
                  <button
                    className="secondary-button"
                    disabled={busy === r.request_id}
                    onClick={() => void decideHost(r.request_id, false)}
                    type="button"
                  >
                    <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>close</span>
                    Reject
                  </button>
                  <button
                    className="connect-button"
                    disabled={busy === r.request_id}
                    onClick={() => void decideHost(r.request_id, true)}
                    type="button"
                  >
                    <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>check</span>
                    {busy === r.request_id ? "…" : "Approve"}
                  </button>
                </div>
              </article>
            ))}
          </div>
        )}
      </section>

      <section className="settings-field" style={{ marginBottom: 20 }}>
        <span>As client — local grants</span>
        <p className="subtitle">
          Legacy local pairing store (daemon). Prefer host approval above for
          cloud.
        </p>
        {pendingPairings.length === 0 ? (
          <p className="empty-state__body">No local client-side requests.</p>
        ) : (
          <div className="pairing-grid">
            {pendingPairings.map((request) => (
              <article className="pairing-card" key={request.requestId}>
                <p className="host-card__label">Local grant</p>
                <h2>{request.hostId.slice(0, 12)}…</h2>
                <p className="host-card__meta">
                  {new Date(request.receivedAt).toLocaleTimeString()}
                </p>
                <div className="pairing-card__actions">
                  <button
                    className="secondary-button"
                    onClick={() => void rejectClient(request)}
                    type="button"
                  >
                    <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>close</span>
                    Reject
                  </button>
                  <button
                    className="connect-button"
                    onClick={() => void acceptClient(request)}
                    type="button"
                  >
                    <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>check</span>
                    Approve
                  </button>
                </div>
              </article>
            ))}
          </div>
        )}
      </section>

      <SharePanel />
    </div>
  );
}
