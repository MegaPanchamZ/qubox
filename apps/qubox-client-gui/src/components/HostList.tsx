import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useApp } from "./AppContext";

type HostListProps = {
  onStartSession: (hostId: string) => void;
};

type LoadState =
  | { kind: "loading" }
  | { kind: "ready" }
  | { kind: "error"; message: string };

export function HostList({ onStartSession }: HostListProps) {
  const {
    knownHosts,
    setKnownHosts,
    discoveredHosts,
    setDiscoveredHosts,
  } = useApp();
  const [knownState, setKnownState] = useState<LoadState>({ kind: "loading" });
  const [discovering, setDiscovering] = useState(false);
  const [discoveryError, setDiscoveryError] = useState<string | null>(null);
  const [actionMessage, setActionMessage] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    const load = async () => {
      try {
        const hosts = await invoke<typeof knownHosts>("get_known_hosts");
        if (cancelled) {
          return;
        }
        setKnownHosts(hosts);
        setKnownState({ kind: "ready" });
      } catch (error) {
        if (cancelled) {
          return;
        }
        setKnownState({
          kind: "error",
          message: String(error),
        });
      }
    };
    void load();
    return () => {
      cancelled = true;
    };
  }, [setKnownHosts]);

  const discover = async () => {
    setDiscovering(true);
    setDiscoveryError(null);
    try {
      const hosts = await invoke<typeof discoveredHosts>("discover_lan_hosts");
      setDiscoveredHosts(hosts);
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

  const handleConnect = async (hostId: string) => {
    setActionMessage(`Launch requested for ${hostId}`);
    try {
      onStartSession(hostId);
    } catch (error) {
      setActionMessage(String(error));
    }
  };

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
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>
              {discovering ? "sync" : "search"}
            </span>
            {discovering ? "Scanning…" : "Discover LAN"}
          </button>
        </div>
      </header>

      {knownState.kind === "loading" ? (
        <p className="state">
          <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>sync</span>
          Loading paired hosts…
        </p>
      ) : null}
      {knownState.kind === "error" ? (
        <p className="state state--error">{knownState.message}</p>
      ) : null}
      {discoveryError ? (
        <p className="state state--error">Discovery failed: {discoveryError}</p>
      ) : null}
      {actionMessage ? <p className="state">{actionMessage}</p> : null}

      <section className="section">
        <h2 className="section__title">Paired hosts</h2>
        {knownState.kind === "ready" && combined.known.length === 0 ? (
          <div className="empty-state">
            <span className="material-symbols-outlined" style={{ fontSize: "2.5rem", color: "var(--muted)", marginBottom: "12px" }}>
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
                    onClick={() => void handleConnect(host.hostPeerId)}
                    type="button"
                  >
                    Connect
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
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>info</span>
            Run a discovery scan to populate this list. Results are kept until
            the next scan.
          </p>
        ) : (
          <div className="host-grid">
            {combined.discovered.map((host) => (
              <article className="host-card host-card--discovered" key={host.peerId}>
                <p className="host-card__label">Discovered</p>
                <h2>{host.deviceName || host.peerId}</h2>
                <p className="host-card__id">{host.peerId}</p>
                <p className="host-card__meta">
                  {host.transports.length > 0
                    ? host.transports.join(" · ")
                    : "transport unknown"}
                </p>
                <div className="host-card__actions">
                  <button
                    className="connect-button"
                    onClick={() => void handleConnect(host.peerId)}
                    type="button"
                  >
                    Pair &amp; connect
                  </button>
                </div>
              </article>
            ))}
          </div>
        )}
      </section>
    </div>
  );
}
