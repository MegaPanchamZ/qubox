import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useApp } from "./AppContext";
import { shareCodeSvg } from "../lib/qr";

type HostListProps = {
  onStartSession: (hostId: string) => Promise<void>;
  onPairAndStartSession: (hostId: string) => Promise<void>;
  onCancelSession?: (sessionId: string) => Promise<void>;
  onKickSession?: (sessionId: string, reason: string) => Promise<void>;
};

type ConnectPhase =
  | { kind: "idle" }
  | { kind: "pairing"; hostId: string }
  | { kind: "waiting_for_approval"; hostId: string }
  | { kind: "launching"; hostId: string }
  | { kind: "error"; hostId: string; message: string };

type ShareLinkPayload = {
  code: string;
  urlHint: string;
  expiresUnixMs: number;
};

export function HostList({
  onStartSession,
  onPairAndStartSession,
  onCancelSession,
  onKickSession,
}: HostListProps) {
  const {
    knownHosts,
    discoveredHosts,
    setDiscoveredHosts,
    refreshKnownHosts,
    ensureNotifications,
    hostRunning,
    refreshHostStatus,
    settings,
    hostPendingPairings,
    removeHostPendingPairing,
    activeSessions,
    telemetryBySession,
  } = useApp();

  const [loadError, setLoadError] = useState<string | null>(null);
  const [discovering, setDiscovering] = useState(false);
  const [discoveryError, setDiscoveryError] = useState<string | null>(null);
  const [connect, setConnect] = useState<ConnectPhase>({ kind: "idle" });
  const [quickInput, setQuickInput] = useState("");
  const [hostBusy, setHostBusy] = useState(false);
  const [hostNotice, setHostNotice] = useState<string | null>(null);
  const [shareCode, setShareCode] = useState<string | null>(null);
  const [shareExpiresMs, setShareExpiresMs] = useState<number | null>(null);
  const [approvingId, setApprovingId] = useState<string | null>(null);
  const [thisComputerName, setThisComputerName] = useState<string>("This Computer");
  const [sessionPerms, setSessionPerms] = useState<
    Record<string, { gamepad: boolean; keyboard: boolean; mouse: boolean }>
  >({});

  const isSharing = hostRunning === true;

  const getPermsForSession = (sessionId: string) =>
    sessionPerms[sessionId] ?? {
      gamepad: true,
      keyboard: false,
      mouse: false,
    };

  const toggleSessionPerm = async (
    sessionId: string,
    field: "gamepad" | "keyboard" | "mouse",
  ) => {
    const current = getPermsForSession(sessionId);
    const updated = { ...current, [field]: !current[field] };
    setSessionPerms((prev) => ({ ...prev, [sessionId]: updated }));
    try {
      await invoke("set_session_permissions", {
        sessionId,
        gamepad: updated.gamepad,
        keyboard: updated.keyboard,
        mouse: updated.mouse,
      });
    } catch {
      // ignore if daemon/host does not support live RPC in mock mode
    }
  };

  const resolveComputerName = (peerId: string): string => {
    const saved = knownHosts.find((h) => h.hostPeerId === peerId);
    if (saved?.displayName) return saved.displayName;
    const disc = discoveredHosts.find((h) => h.peerId === peerId);
    if (disc?.deviceName) return disc.deviceName;
    return "Remote Computer";
  };

  useEffect(() => {
    const load = async () => {
      try {
        await refreshKnownHosts();
        await refreshHostStatus();
        try {
          const ob = await invoke<{ deviceName?: string | null }>("get_onboarding");
          if (ob?.deviceName?.trim()) {
            setThisComputerName(ob.deviceName.trim());
          }
        } catch {
          // ignore if onboarding query fails in mock environments
        }
        setLoadError(null);
      } catch (error) {
        setLoadError(String(error));
      }
    };
    void load();
  }, [refreshKnownHosts, refreshHostStatus]);

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

  const toggleHostSharing = async () => {
    setHostBusy(true);
    setHostNotice(null);
    try {
      if (isSharing) {
        await invoke("stop_host_agent");
        setHostNotice("Screen sharing stopped.");
      } else {
        await invoke("start_host_agent", {});
        setHostNotice("This computer is now ready for remote connections.");
      }
      await refreshHostStatus();
    } catch (error) {
      setLoadError(String(error));
    } finally {
      setHostBusy(false);
    }
  };

  const generateShareCode = async () => {
    setHostNotice(null);
    try {
      const res = await invoke<ShareLinkPayload>("create_share_link", {
        ttlSecs: 900,
      });
      setShareCode(res.code);
      setShareExpiresMs(res.expiresUnixMs || null);
      setHostNotice(
        "Party Share Code created (valid 15 minutes — up to 4 friends can join with controllers).",
      );
    } catch (error) {
      setLoadError(`Could not create share code: ${String(error)}`);
    }
  };

  const decideIncomingPairing = async (requestId: string, approved: boolean) => {
    setApprovingId(requestId);
    try {
      await invoke("host_pairing_decide", { requestId, approved });
      removeHostPendingPairing(requestId);
      if (approved) {
        await refreshKnownHosts();
      }
      setHostNotice(
        approved
          ? "Approved connection request."
          : "Declined connection request.",
      );
    } catch (error) {
      setLoadError(`Approval failed: ${String(error)}`);
    } finally {
      setApprovingId(null);
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

  const handleQuickConnect = async () => {
    const target = quickInput.trim();
    if (!target) return;

    // A short alphanumeric code without hyphens (6-12 chars) is a Party Share Code
    const looksLikeShareCode =
      /^[A-Za-z0-9]{6,12}$/.test(target) && !target.includes("-");
    if (looksLikeShareCode) {
      setConnect({ kind: "pairing", hostId: target });
      try {
        const out = await invoke<string>("redeem_share_link", { code: target });
        await refreshKnownHosts();
        setQuickInput("");
        // Extract host=<uuid> if returned and auto-launch the co-op session
        const match = /host=([0-9a-fA-F-]{36})/.exec(out ?? "");
        if (match?.[1]) {
          setHostNotice("Joined party! Launching game stream…");
          await onStartSession(match[1]);
        } else {
          setHostNotice(
            out
              ? `Joined party (${out}).`
              : "Party code redeemed! You can now click Connect below.",
          );
        }
        setConnect({ kind: "idle" });
      } catch (error) {
        setConnect({ kind: "error", hostId: target, message: String(error) });
      }
      return;
    }

    // Otherwise check if already in knownHosts; if not, Pair & Connect
    const alreadyPaired = knownHosts.some((h) => h.hostPeerId === target);
    await handleConnect(target, !alreadyPaired);
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

  const qrSvg = shareCode ? shareCodeSvg(shareCode) : null;

  return (
    <div className="view" data-testid="computers-view">
      <header className="view__header">
        <div>
          <p className="eyebrow">Qubox Remote Desktop</p>
          <h1>Computers</h1>
          <p className="subtitle">
            Share this screen or connect to another computer anywhere over Iroh
            P2P.
          </p>
        </div>
        <div className="view__actions">
          <button
            className="secondary-button"
            data-testid="discover-button"
            disabled={discovering}
            onClick={() => void discover()}
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
              {discovering ? "sync" : "refresh"}
            </span>
            {discovering ? "Scanning…" : "Scan for Computers"}
          </button>
        </div>
      </header>

      {/* 1. Incoming Pairing Requests Banner */}
      {hostPendingPairings.length > 0 ? (
        <section
          className="host-card"
          data-testid="incoming-pairing-banner"
          style={{
            borderColor: "var(--primary)",
            background: "rgba(255, 185, 81, 0.08)",
          }}
        >
          <p className="host-card__label">Incoming Connection Request</p>
          {hostPendingPairings.map((r) => (
            <div
              key={r.request_id}
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: 16,
                flexWrap: "wrap",
              }}
            >
              <div>
                <h2 style={{ margin: "0 0 4px" }}>
                  {r.client_name || r.client_label || "Another Computer"} wants
                  to connect
                </h2>
                <p className="host-card__meta">
                  Approve to allow{" "}
                  {r.client_name || r.client_label || "this computer"} to view
                  and control your screen.
                </p>
              </div>
              <div style={{ display: "flex", gap: 10 }}>
                <button
                  className="secondary-button"
                  disabled={approvingId === r.request_id}
                  onClick={() => void decideIncomingPairing(r.request_id, false)}
                  type="button"
                >
                  Decline
                </button>
                <button
                  className="connect-button"
                  data-testid={`approve-pairing-${r.request_id}`}
                  disabled={approvingId === r.request_id}
                  onClick={() => void decideIncomingPairing(r.request_id, true)}
                  type="button"
                >
                  Approve &amp; Pair
                </button>
              </div>
            </div>
          ))}
        </section>
      ) : null}

      {/* 2. Active Sessions Banner */}
      {activeSessions.length > 0 ? (
        <section
          className="host-card"
          data-testid="active-sessions-banner"
          style={{
            borderColor: "var(--success)",
            background: "rgba(52, 211, 153, 0.07)",
            display: "flex",
            flexDirection: "column",
            gap: 14,
          }}
        >
          <p
            className="host-card__label"
            style={{ color: "var(--success)", margin: 0 }}
          >
            Active Remote Session ({activeSessions.length}) · Co-Op Party Ready
          </p>
          {activeSessions.map((session, idx) => {
            const events = telemetryBySession[session.sessionId] ?? [];
            const decodedCount = events.filter(
              (e) => e.kind === "frame_decoded",
            ).length;
            const computerName = resolveComputerName(session.hostId);
            const perms = getPermsForSession(session.sessionId);
            const playerSlot = (idx % 4) + 1;
            return (
              <div
                key={session.sessionId}
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: 16,
                  flexWrap: "wrap",
                  paddingTop: idx > 0 ? 12 : 0,
                  borderTop:
                    idx > 0 ? "1px solid rgba(255,255,255,0.08)" : "none",
                }}
              >
                <div>
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: 10,
                      marginBottom: 4,
                    }}
                  >
                    <span
                      data-testid={`player-slot-badge-${playerSlot}`}
                      style={{
                        display: "inline-flex",
                        alignItems: "center",
                        gap: 5,
                        padding: "3px 10px",
                        borderRadius: 999,
                        fontSize: "0.75rem",
                        fontWeight: 800,
                        letterSpacing: "0.06em",
                        background: "rgba(255, 185, 81, 0.18)",
                        color: "var(--primary)",
                        border: "1px solid rgba(255, 185, 81, 0.45)",
                      }}
                    >
                      PLAYER {playerSlot}
                    </span>
                    <h2 style={{ margin: 0 }}>Connected to {computerName}</h2>
                  </div>
                  <p className="host-card__meta">
                    Streaming live over Iroh P2P · {decodedCount} frames decoded
                  </p>
                </div>

                {/* Per-Friend Co-Op Input Controls */}
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 8,
                    flexWrap: "wrap",
                  }}
                >
                  <button
                    className="secondary-button"
                    data-testid={`toggle-perm-gamepad-${session.sessionId}`}
                    onClick={() =>
                      void toggleSessionPerm(session.sessionId, "gamepad")
                    }
                    style={{
                      borderColor: perms.gamepad
                        ? "var(--success)"
                        : "var(--panel-border)",
                      background: perms.gamepad
                        ? "rgba(52, 211, 153, 0.16)"
                        : "rgba(255, 255, 255, 0.03)",
                      color: perms.gamepad ? "#ecfdf5" : "var(--muted)",
                      padding: "7px 12px",
                      fontSize: "0.78rem",
                    }}
                    title="Allow or block virtual Xbox 360 controller input for this friend"
                    type="button"
                  >
                    Controller: {perms.gamepad ? "ON" : "OFF"}
                  </button>

                  <button
                    className="secondary-button"
                    data-testid={`toggle-perm-keyboard-${session.sessionId}`}
                    onClick={() =>
                      void toggleSessionPerm(session.sessionId, "keyboard")
                    }
                    style={{
                      borderColor: perms.keyboard
                        ? "var(--success)"
                        : "var(--panel-border)",
                      background: perms.keyboard
                        ? "rgba(52, 211, 153, 0.16)"
                        : "rgba(255, 255, 255, 0.03)",
                      color: perms.keyboard ? "#ecfdf5" : "var(--muted)",
                      padding: "7px 12px",
                      fontSize: "0.78rem",
                    }}
                    title="Allow or block keyboard input for this friend"
                    type="button"
                  >
                    Keyboard: {perms.keyboard ? "ON" : "OFF"}
                  </button>

                  <button
                    className="secondary-button"
                    data-testid={`toggle-perm-mouse-${session.sessionId}`}
                    onClick={() =>
                      void toggleSessionPerm(session.sessionId, "mouse")
                    }
                    style={{
                      borderColor: perms.mouse
                        ? "var(--success)"
                        : "var(--panel-border)",
                      background: perms.mouse
                        ? "rgba(52, 211, 153, 0.16)"
                        : "rgba(255, 255, 255, 0.03)",
                      color: perms.mouse ? "#ecfdf5" : "var(--muted)",
                      padding: "7px 12px",
                      fontSize: "0.78rem",
                    }}
                    title="Allow or block mouse control for this friend"
                    type="button"
                  >
                    Mouse: {perms.mouse ? "ON" : "OFF"}
                  </button>

                  {onKickSession ? (
                    <button
                      className="secondary-button"
                      onClick={() =>
                        void onKickSession(
                          session.sessionId,
                          "disconnected by user",
                        )
                      }
                      type="button"
                    >
                      Kick Viewer
                    </button>
                  ) : null}
                  {onCancelSession ? (
                    <button
                      className="danger-button"
                      data-testid="disconnect-session-button"
                      onClick={() => void onCancelSession(session.sessionId)}
                      type="button"
                    >
                      Disconnect
                    </button>
                  ) : null}
                </div>
              </div>
            );
          })}
        </section>
      ) : null}

      {loadError ? <p className="state state--error">{loadError}</p> : null}
      {discoveryError ? (
        <p className="state state--error">Scan failed: {discoveryError}</p>
      ) : null}
      {hostNotice ? <p className="state state--info">{hostNotice}</p> : null}
      {connect.kind === "waiting_for_approval" ? (
        <p className="state state--info" data-testid="pair-pending">
          Waiting for <strong>{resolveComputerName(connect.hostId)}</strong> to
          click <strong>Approve</strong>…
          <button
            className="secondary-button"
            onClick={() => cancelPendingPair(connect.hostId)}
            type="button"
            style={{ marginLeft: 8 }}
          >
            Cancel
          </button>
        </p>
      ) : null}
      {connect.kind === "error" ? (
        <p className="state state--error">Connection failed: {connect.message}</p>
      ) : null}

      {/* 3. THIS COMPUTER (Host Sharing Card) */}
      <section className="section">
        <h2 className="section__title">This Computer (Share Screen &amp; Co-Op Host)</h2>
        <div
          className="host-card"
          data-testid="this-computer-card"
          style={{
            display: "flex",
            flexDirection: "column",
            gap: 18,
          }}
        >
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: 16,
              flexWrap: "wrap",
            }}
          >
            <div>
              <p className="host-card__label" style={{ margin: 0 }}>
                {isSharing
                  ? "Ready for remote connections & co-op gaming"
                  : "Screen sharing is off"}
              </p>
              <h2 style={{ margin: "6px 0 4px" }}>{thisComputerName}</h2>
              <p className="host-card__meta">
                {isSharing
                  ? "Invite up to 4 friends with a Party Code (controllers auto-assigned to Player 1–4)."
                  : "Start sharing to play local co-op games with friends or access this PC remotely."}
              </p>
            </div>

            <div style={{ display: "flex", gap: 10, flexWrap: "wrap" }}>
              {settings?.hostPeerId ? (
                <button
                  className="secondary-button"
                  data-testid="copy-host-id"
                  onClick={() => {
                    if (settings.hostPeerId) {
                      void navigator.clipboard.writeText(settings.hostPeerId);
                      setHostNotice("Copied Your Computer ID to clipboard.");
                    }
                  }}
                  type="button"
                >
                  <span
                    className="material-symbols-outlined"
                    style={{ fontSize: "1.05rem" }}
                  >
                    content_copy
                  </span>
                  Copy ID
                </button>
              ) : null}

              <button
                className="secondary-button"
                data-testid="create-share-code"
                onClick={() => void generateShareCode()}
                type="button"
              >
                <span
                  className="material-symbols-outlined"
                  style={{ fontSize: "1.05rem" }}
                >
                  sports_esports
                </span>
                Invite Friends (Party Code)
              </button>

              <button
                className={isSharing ? "danger-button" : "primary-button"}
                data-testid="toggle-host-sharing"
                disabled={hostBusy}
                onClick={() => void toggleHostSharing()}
                type="button"
              >
                <span
                  className="material-symbols-outlined"
                  style={{ fontSize: "1.1rem" }}
                >
                  {isSharing ? "stop_circle" : "cast"}
                </span>
                {hostBusy
                  ? "Working…"
                  : isSharing
                    ? "Stop Sharing"
                    : "Start Sharing"}
              </button>
            </div>
          </div>

          {shareCode ? (
            <div
              className="share-code-box"
              data-testid="share-code-display"
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: 20,
                flexWrap: "wrap",
                padding: 16,
                borderRadius: 14,
                background: "rgba(0, 0, 0, 0.35)",
                border: "1px solid var(--panel-border)",
              }}
            >
              <div>
                <p className="host-card__label">
                  15-Minute Party Code (Up to 4 Friends · Controller Ready)
                </p>
                <code
                  className="share-code"
                  style={{
                    fontSize: "1.6rem",
                    fontWeight: 800,
                    letterSpacing: "0.14em",
                    color: "var(--primary)",
                  }}
                >
                  {shareCode}
                </code>
                {shareExpiresMs ? (
                  <p className="host-card__meta" style={{ marginTop: 4 }}>
                    Expires at {new Date(shareExpiresMs).toLocaleTimeString()} ·
                    Friends get Controller ON, Keyboard &amp; Mouse OFF by default
                  </p>
                ) : null}
                <div style={{ marginTop: 10, display: "flex", gap: 8 }}>
                  <button
                    className="secondary-button"
                    onClick={() => {
                      void navigator.clipboard.writeText(shareCode);
                      setHostNotice("Copied party code to clipboard.");
                    }}
                    type="button"
                  >
                    Copy Party Code
                  </button>
                  <button
                    className="ghost-button"
                    onClick={() => setShareCode(null)}
                    type="button"
                  >
                    Hide
                  </button>
                </div>
              </div>
              {qrSvg ? (
                <div
                  className="share-qr"
                  aria-label="Share code QR"
                  role="img"
                  style={{ width: 120, height: 120 }}
                  dangerouslySetInnerHTML={{ __html: qrSvg }}
                />
              ) : null}
            </div>
          ) : null}
        </div>
      </section>

      {/* 4. CONNECT TO ANOTHER COMPUTER */}
      <section className="section">
        <h2 className="section__title">Connect to Another Computer</h2>

        {/* Quick Connect Bar */}
        <div
          className="host-card"
          style={{
            display: "flex",
            flexDirection: "row",
            gap: 12,
            flexWrap: "wrap",
            alignItems: "center",
            padding: "18px 22px",
          }}
        >
          <input
            className="text-input"
            data-testid="manual-host-id"
            onChange={(e) => setQuickInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && quickInput.trim()) {
                void handleQuickConnect();
              }
            }}
            placeholder="Paste a Share Code (e.g. A7K9M2) or Computer ID…"
            style={{ flex: 1, minWidth: 240 }}
            value={quickInput}
          />
          <button
            className="connect-button"
            data-testid="quick-connect-button"
            disabled={!quickInput.trim() || isBusyFor(quickInput.trim())}
            onClick={() => void handleQuickConnect()}
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.1rem" }}
            >
              bolt
            </span>
            Connect
          </button>
        </div>

        {/* Unified Computers Grid */}
        {combined.known.length === 0 && combined.discovered.length === 0 ? (
          <div className="empty-state" data-testid="empty-computers-state">
            <span
              className="material-symbols-outlined"
              style={{
                fontSize: "2.4rem",
                color: "var(--muted)",
                marginBottom: "10px",
              }}
            >
              devices
            </span>
            <p className="empty-state__title">No other computers found yet</p>
            <p className="empty-state__body">
              Click <strong>Scan for Computers</strong> above to find online
              computers on your server, or paste a Share Code / Computer ID.
            </p>
          </div>
        ) : (
          <div className="host-grid" data-testid="computers-grid">
            {combined.known.map((host) => (
              <article className="host-card" key={host.hostPeerId}>
                <p className="host-card__label">Saved Computer</p>
                <h2>{host.displayName ?? "Paired Computer"}</h2>
                <p className="host-card__meta">
                  Paired &amp; trusted · Ready for 1-click remote control
                </p>
                <div className="host-card__actions">
                  <button
                    className="connect-button"
                    data-testid={`connect-known-${host.hostPeerId}`}
                    disabled={isBusyFor(host.hostPeerId)}
                    onClick={() => void handleConnect(host.hostPeerId, false)}
                    type="button"
                  >
                    {connect.kind === "launching" &&
                    connect.hostId === host.hostPeerId
                      ? "Connecting…"
                      : "Connect"}
                  </button>
                </div>
              </article>
            ))}

            {combined.discovered.map((host) => {
              const waiting = isBusyFor(host.peerId);
              return (
                <article
                  className="host-card host-card--discovered"
                  key={host.peerId}
                >
                  <p className="host-card__label">Online Now</p>
                  <h2>{host.deviceName || "Remote Computer"}</h2>
                  <p className="host-card__meta">
                    Available on your server · Click Pair &amp; Connect
                  </p>
                  <div className="host-card__actions">
                    {waiting ? (
                      <>
                        <button className="connect-button" disabled type="button">
                          Waiting for approval…
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
                        data-testid={`pair-connect-${host.peerId}`}
                        disabled={isBusyFor(host.peerId)}
                        onClick={() => void handleConnect(host.peerId, true)}
                        type="button"
                      >
                        Pair &amp; Connect
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
