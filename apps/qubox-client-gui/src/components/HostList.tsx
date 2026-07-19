import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useApp } from "./AppContext";

type HostListProps = {
  onStartSession: (hostId: string) => Promise<void>;
  onPairAndStartSession: (hostId: string) => Promise<void>;
};

type ConnectPhase =
  | { kind: "idle" }
  | { kind: "pairing"; hostId: string }
  | { kind: "waiting_for_approval"; hostId: string }
  | { kind: "launching"; hostId: string }
  | { kind: "error"; hostId: string; message: string };

export function HostList({
  onStartSession,
  onPairAndStartSession,
}: HostListProps) {
  const {
    knownHosts,
    discoveredHosts,
    setDiscoveredHosts,
    refreshKnownHosts,
    ensureNotifications,
  } = useApp();
  const [loadError, setLoadError] = useState<string | null>(null);
  const [discovering, setDiscovering] = useState(false);
  const [discoveryError, setDiscoveryError] = useState<string | null>(null);
  const [connect, setConnect] = useState<ConnectPhase>({ kind: "idle" });

  useEffect(() => {
    const load = async () => {
      try {
        await refreshKnownHosts();
        setLoadError(null);
      } catch (error) {
        setLoadError(String(error));
      }
    };
    void load();
  }, [refreshKnownHosts]);

  const discover = async () => {
    setDiscovering(true);
    setDiscoveryError(null);
    try {
      const hosts =
        await invoke<
          { peer_id: string; device_name: string; transports: string[] }[]
        >("discover_lan_hosts");
      setDiscoveredHosts(
        hosts.map((h) => ({
          peerId: h.peer_id,
          deviceName: h.device_name,
          transports: h.transports,
          discoveredAt: Date.now(),
        })),
      );
    } catch (error) {
      setDiscoveryError(String(error));
    } finally {
      setDiscovering(false);
    }
  };

  const combined = useMemo(() => {
    const known = new Set(knownHosts.map((h) => h.hostPeerId));
    return {
      known: knownHosts,
      discovered: discoveredHosts.filter((h) => !known.has(h.peerId)),
    };
  }, [knownHosts, discoveredHosts]);

  const handleConnect = async (hostId: string, pair = false) => {
    setConnect(
      pair ? { kind: "pairing", hostId } : { kind: "launching", hostId },
    );
    try {
      if (pair) {
        setConnect({ kind: "waiting_for_approval", hostId });
        await ensureNotifications();
        await onPairAndStartSession(hostId);
      } else {
        await onStartSession(hostId);
      }
      setConnect({ kind: "idle" });
    } catch (error) {
      setConnect({ kind: "error", hostId, message: String(error) });
    }
  };

  const cancelPendingPair = (hostId: string) => {
    if (connect.kind === "waiting_for_approval" && connect.hostId === hostId) {
      setConnect({ kind: "idle" });
    }
  };

  const isBusyFor = (hostId: string) =>
    connect.kind !== "idle" &&
    connect.kind !== "error" &&
    connect.hostId === hostId;

  const ttlSeconds = 4 * 60;
  const loading = knownHosts.length === 0 && loadError === null;

  return (
    <div className="view">
      <header className="view__header">
        <div>
          <p className="eyebrow">Hosts</p>
          <h1>Connect to a host</h1>
          <p className="subtitle">
            Pair once, then connect from anywhere. Use Discover to find hosts on
            the local network.
          </p>
        </div>
        <div className="view__actions">
          <button
            className="secondary-button"
            disabled={discovering}
            onClick={() => void discover()}
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
              {discovering ? "sync" : "search"}
            </span>
            {discovering ? "Scanning…" : "Discover LAN"}
          </button>
        </div>
      </header>

      {loading ? (
        <p className="state">
          <span
            className="material-symbols-outlined"
            style={{ fontSize: "1.1rem" }}
          >
            sync
          </span>
          Loading paired hosts…
        </p>
      ) : null}
      {loadError ? <p className="state state--error">{loadError}</p> : null}
      {discoveryError ? (
        <p className="state state--error">Discovery failed: {discoveryError}</p>
      ) : null}
      {connect.kind === "waiting_for_approval" ? (
        <p className="state state--info" data-testid="pair-pending">
          Waiting for host to accept the pairing request{" "}
          <code>{connect.hostId.slice(0, 12)}…</code>
          <button
            className="secondary-button"
            onClick={() => cancelPendingPair(connect.hostId)}
            type="button"
            style={{ marginLeft: 8 }}
          >
            Cancel request
          </button>
        </p>
      ) : null}
      {connect.kind === "error" ? (
        <p className="state state--error">Connect failed: {connect.message}</p>
      ) : null}

      <section className="section">
        <h2 className="section__title">Paired hosts</h2>
        {!loading && combined.known.length === 0 ? (
          <div className="empty-state">
            <span
              className="material-symbols-outlined"
              style={{
                fontSize: "2.5rem",
                color: "var(--muted)",
                marginBottom: "12px",
              }}
            >
              sensors_off
            </span>
            <p className="empty-state__title">No paired hosts yet</p>
            <p className="empty-state__body">
              Pair a host from the Pairing tab. Existing CLI pairings will show
              up here automatically.
            </p>
          </div>
        ) : null}
        {combined.known.length > 0 ? (
          <div className="host-grid">
            {combined.known.map((host) => (
              <article className="host-card" key={host.hostPeerId}>
                <p className="host-card__label">Paired Host</p>
                <h2>{host.displayName ?? "Display name unavailable"}</h2>
                <p className="host-card__id">{host.hostPeerId}</p>
                <div className="host-card__actions">
                  <button
                    className="connect-button"
                    disabled={isBusyFor(host.hostPeerId)}
                    onClick={() => void handleConnect(host.hostPeerId)}
                    type="button"
                  >
                    {connect.kind === "launching" &&
                    connect.hostId === host.hostPeerId
                      ? "Launching…"
                      : "Connect"}
                  </button>
                </div>
              </article>
            ))}
          </div>
        ) : null}
      </section>

      <section className="section">
        <h2 className="section__title">Discovered on LAN</h2>
        {combined.discovered.length === 0 ? (
          <p className="state">
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
              info
            </span>
            Run a discovery scan to populate this list. Results expire after{" "}
            {Math.round(ttlSeconds / 60)} minutes.
          </p>
        ) : (
          <div className="host-grid">
            {combined.discovered.map((host) => {
              const ageSec = Math.max(
                0,
                Math.round((Date.now() - host.discoveredAt) / 1000),
              );
              const waiting = isBusyFor(host.peerId);
              return (
                <article
                  className="host-card host-card--discovered"
                  key={host.peerId}
                >
                  <p className="host-card__label">Discovered</p>
                  <h2>{host.deviceName || host.peerId}</h2>
                  <p className="host-card__id">{host.peerId}</p>
                  <p className="host-card__meta">
                    {host.transports.length > 0
                      ? host.transports.join(" · ")
                      : "transport unknown"}
                  </p>
                  <p className="host-card__meta">seen {ageSec}s ago</p>
                  <div className="host-card__actions">
                    {waiting ? (
                      <>
                        <button
                          className="connect-button"
                          disabled
                          type="button"
                        >
                          Waiting for host…
                        </button>
                        <button
                          className="secondary-button"
                          onClick={() => cancelPendingPair(host.peerId)}
                          type="button"
                        >
                          Cancel
                        </button>
                      </>
                    ) : (
                      <button
                        className="connect-button"
                        disabled={isBusyFor(host.peerId)}
                        onClick={() => void handleConnect(host.peerId, true)}
                        type="button"
                      >
                        Pair &amp; connect
                      </button>
                    )}
                  </div>
                </article>
              );
            })}
          </div>
        )}
      </section>
    </div>
  );
}
