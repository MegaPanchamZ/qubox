import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useApp } from "./AppContext";

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

type PeerPermissionEntry = {
  gamepad: boolean;
  keyboard: boolean;
  mouse: boolean;
  autoApprove: boolean;
};

const DEFAULT_PEER_PERMS: PeerPermissionEntry = {
  gamepad: true,
  keyboard: false,
  mouse: false,
  autoApprove: false,
};

const iconBtnStyle = (active = false, danger = false): React.CSSProperties => ({
  display: "inline-flex",
  alignItems: "center",
  justifyContent: "center",
  width: 36,
  height: 36,
  borderRadius: 10,
  border: `1px solid ${
    danger
      ? "rgba(255, 180, 171, 0.3)"
      : active
        ? "var(--success)"
        : "var(--panel-border)"
  }`,
  background: danger
    ? "rgba(255, 180, 171, 0.1)"
    : active
      ? "rgba(52, 211, 153, 0.16)"
      : "rgba(255, 255, 255, 0.03)",
  color: danger ? "var(--danger)" : active ? "var(--success)" : "var(--muted)",
  cursor: "pointer",
  padding: 0,
  transition: "all 0.15s ease",
});

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
  } = useApp();

  const [loadError, setLoadError] = useState<string | null>(null);
  const [discovering, setDiscovering] = useState(false);
  const [discoveryError, setDiscoveryError] = useState<string | null>(null);
  const [connect, setConnect] = useState<ConnectPhase>({ kind: "idle" });
  const [quickInput, setQuickInput] = useState("");
  const [hostBusy, setHostBusy] = useState(false);
  const [hostNotice, setHostNotice] = useState<string | null>(null);
  const [shareCode, setShareCode] = useState<string | null>(null);
  const [approvingId, setApprovingId] = useState<string | null>(null);
  const [thisComputerName, setThisComputerName] = useState<string>("This Computer");
  const [peerPerms, setPeerPerms] = useState<Record<string, PeerPermissionEntry>>({});
  const [sessionPerms, setSessionPerms] = useState<
    Record<string, { gamepad: boolean; keyboard: boolean; mouse: boolean }>
  >({});

  const isSharing = hostRunning === true;

  const getPermsForPeer = (peerId: string): PeerPermissionEntry =>
    peerPerms[peerId] ?? DEFAULT_PEER_PERMS;

  const getPermsForSession = (sessionId: string, peerId?: string) => {
    if (sessionPerms[sessionId]) return sessionPerms[sessionId];
    if (peerId && peerPerms[peerId]) {
      const p = peerPerms[peerId];
      return { gamepad: p.gamepad, keyboard: p.keyboard, mouse: p.mouse };
    }
    return {
      gamepad: DEFAULT_PEER_PERMS.gamepad,
      keyboard: DEFAULT_PEER_PERMS.keyboard,
      mouse: DEFAULT_PEER_PERMS.mouse,
    };
  };

  const togglePeerPerm = async (
    peerId: string,
    field: keyof PeerPermissionEntry,
  ) => {
    const current = getPermsForPeer(peerId);
    const updated: PeerPermissionEntry = {
      ...current,
      [field]: !current[field],
    };
    setPeerPerms((prev) => ({ ...prev, [peerId]: updated }));
    try {
      await invoke("set_peer_permissions", {
        peerId,
        gamepad: updated.gamepad,
        keyboard: updated.keyboard,
        mouse: updated.mouse,
        autoApprove: updated.autoApprove,
      });
    } catch {
      // ignore in mock mode
    }
    // Also sync to any active sessions for this peer
    for (const s of activeSessions) {
      if (s.hostId === peerId) {
        setSessionPerms((prev) => ({
          ...prev,
          [s.sessionId]: {
            gamepad: updated.gamepad,
            keyboard: updated.keyboard,
            mouse: updated.mouse,
          },
        }));
        try {
          await invoke("set_session_permissions", {
            sessionId: s.sessionId,
            gamepad: updated.gamepad,
            keyboard: updated.keyboard,
            mouse: updated.mouse,
          });
        } catch {
          // ignore
        }
      }
    }
  };

  const toggleSessionPerm = async (
    sessionId: string,
    peerId: string,
    field: "gamepad" | "keyboard" | "mouse",
  ) => {
    const current = getPermsForSession(sessionId, peerId);
    const updated = { ...current, [field]: !current[field] };
    setSessionPerms((prev) => ({ ...prev, [sessionId]: updated }));
    const existingPeer = getPermsForPeer(peerId);
    const updatedPeer: PeerPermissionEntry = {
      ...existingPeer,
      gamepad: updated.gamepad,
      keyboard: updated.keyboard,
      mouse: updated.mouse,
    };
    setPeerPerms((prev) => ({ ...prev, [peerId]: updatedPeer }));
    try {
      await invoke("set_session_permissions", {
        sessionId,
        gamepad: updated.gamepad,
        keyboard: updated.keyboard,
        mouse: updated.mouse,
      });
      await invoke("set_peer_permissions", {
        peerId,
        gamepad: updatedPeer.gamepad,
        keyboard: updatedPeer.keyboard,
        mouse: updatedPeer.mouse,
        autoApprove: updatedPeer.autoApprove,
      });
    } catch {
      // ignore if daemon/host does not support live RPC in mock mode
    }
  };

  const removeSavedHost = async (peerId: string) => {
    try {
      await invoke("reject_pairing", { hostId: peerId });
      await refreshKnownHosts();
    } catch (error) {
      setLoadError(`Remove failed: ${String(error)}`);
    }
  };

  const kickAllSessions = async () => {
    for (const s of activeSessions) {
      if (onCancelSession) {
        await onCancelSession(s.sessionId);
      } else if (onKickSession) {
        await onKickSession(s.sessionId, "disconnected by host");
      }
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
        try {
          const savedPerms =
            await invoke<Record<string, Partial<PeerPermissionEntry>>>(
              "get_peer_permissions",
            );
          if (savedPerms && typeof savedPerms === "object") {
            const normalized: Record<string, PeerPermissionEntry> = {};
            for (const [k, v] of Object.entries(savedPerms)) {
              normalized[k] = {
                gamepad: v?.gamepad ?? true,
                keyboard: v?.keyboard ?? false,
                mouse: v?.mouse ?? false,
                autoApprove: v?.autoApprove ?? false,
              };
            }
            setPeerPerms(normalized);
          }
        } catch {
          // ignore
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
      } else {
        await invoke("start_host_agent", {});
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
    } catch (error) {
      setLoadError(`Could not create code: ${String(error)}`);
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

  const filtered = useMemo(() => {
    const q = quickInput.trim().toLowerCase();
    if (!q) return combined;
    return {
      known: combined.known.filter(
        (h) =>
          (h.displayName ?? "").toLowerCase().includes(q) ||
          h.hostPeerId.toLowerCase().includes(q),
      ),
      discovered: combined.discovered.filter(
        (h) =>
          (h.deviceName ?? "").toLowerCase().includes(q) ||
          h.peerId.toLowerCase().includes(q),
      ),
    };
  }, [combined, quickInput]);

  const totalCount = combined.known.length + combined.discovered.length;
  const shownCount = filtered.known.length + filtered.discovered.length;

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

    // If the user typed a search filter that matches a computer in the list, connect to the top match
    if (filtered.known.length > 0) {
      await handleConnect(filtered.known[0].hostPeerId, false);
      return;
    }
    if (filtered.discovered.length > 0) {
      await handleConnect(filtered.discovered[0].peerId, true);
      return;
    }

    const looksLikeShareCode =
      /^[A-Za-z0-9]{6,12}$/.test(target) && !target.includes("-");
    if (looksLikeShareCode) {
      setConnect({ kind: "pairing", hostId: target });
      try {
        const out = await invoke<string>("redeem_share_link", { code: target });
        await refreshKnownHosts();
        setQuickInput("");
        const match = /host=([0-9a-fA-F-]{36})/.exec(out ?? "");
        if (match?.[1]) {
          await onStartSession(match[1]);
        }
        setConnect({ kind: "idle" });
      } catch (error) {
        setConnect({ kind: "error", hostId: target, message: String(error) });
      }
      return;
    }

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

  return (
    <div
      className="view"
      data-testid="computers-view"
      style={{
        gap: 14,
        height: "calc(100vh - 88px)",
        display: "flex",
        flexDirection: "column",
        overflow: "hidden",
      }}
    >
      <header
        className="view__header"
        style={{ paddingBottom: 12, alignItems: "center", flexShrink: 0 }}
      >
        <div style={{ display: "flex", alignItems: "baseline", gap: 10 }}>
          <h1 style={{ fontSize: "1.85rem" }}>Computers</h1>
          {totalCount > 0 ? (
            <span
              data-testid="computers-count"
              style={{ color: "var(--muted)", fontSize: "0.9rem" }}
            >
              {quickInput.trim()
                ? `${shownCount} of ${totalCount}`
                : totalCount}
            </span>
          ) : null}
        </div>
        <div className="view__actions">
          <button
            data-testid="discover-button"
            disabled={discovering}
            onClick={() => void discover()}
            style={iconBtnStyle(false)}
            title="Refresh computers"
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.15rem" }}
            >
              {discovering ? "sync" : "refresh"}
            </span>
          </button>
        </div>
      </header>

      {/* 1. Incoming Pairing Requests (Compact Single-Line Rows) */}
      {hostPendingPairings.length > 0 ? (
        <section
          className="host-card"
          data-testid="incoming-pairing-banner"
          style={{
            borderColor: "var(--primary)",
            background: "rgba(255, 185, 81, 0.06)",
            padding: "10px 16px",
            display: "flex",
            flexDirection: "column",
            gap: 8,
          }}
        >
          {hostPendingPairings.map((r) => (
            <div
              key={r.request_id}
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: 12,
              }}
            >
              <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
                <strong style={{ fontSize: "0.95rem", color: "#fff" }}>
                  {r.client_name || r.client_label || "Another Computer"}
                </strong>
                <span style={{ color: "var(--muted)", fontSize: "0.85rem" }}>
                  Incoming request
                </span>
              </div>
              <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                <button
                  disabled={approvingId === r.request_id}
                  onClick={() => void decideIncomingPairing(r.request_id, false)}
                  style={iconBtnStyle(false, true)}
                  title="Decline"
                  type="button"
                >
                  <span
                    className="material-symbols-outlined"
                    style={{ fontSize: "1.1rem" }}
                  >
                    close
                  </span>
                </button>
                <button
                  data-testid={`approve-pairing-${r.request_id}`}
                  disabled={approvingId === r.request_id}
                  onClick={() => void decideIncomingPairing(r.request_id, true)}
                  style={iconBtnStyle(true, false)}
                  title="Approve"
                  type="button"
                >
                  <span
                    className="material-symbols-outlined"
                    style={{ fontSize: "1.1rem" }}
                  >
                    check
                  </span>
                </button>
              </div>
            </div>
          ))}
        </section>
      ) : null}

      {/* 2. Active Sessions (Parsec-Style Compact Icon Rows) */}
      {activeSessions.length > 0 ? (
        <section
          className="host-card"
          data-testid="active-sessions-banner"
          style={{
            borderColor: "rgba(52, 211, 153, 0.35)",
            background: "rgba(52, 211, 153, 0.05)",
            padding: "10px 16px",
            display: "flex",
            flexDirection: "column",
            gap: 8,
          }}
        >
          {activeSessions.map((session, idx) => {
            const computerName = resolveComputerName(session.hostId);
            const perms = getPermsForSession(session.sessionId, session.hostId);
            const playerSlot = (idx % 4) + 1;
            return (
              <div
                key={session.sessionId}
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: 12,
                  paddingTop: idx > 0 ? 8 : 0,
                  borderTop:
                    idx > 0 ? "1px solid rgba(255,255,255,0.06)" : "none",
                }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
                  <span
                    data-testid={`player-slot-badge-${playerSlot}`}
                    style={{
                      padding: "2px 8px",
                      borderRadius: 6,
                      fontSize: "0.72rem",
                      fontWeight: 800,
                      background: "rgba(255, 185, 81, 0.16)",
                      color: "var(--primary)",
                    }}
                  >
                    P{playerSlot}
                  </span>
                  <strong style={{ fontSize: "0.95rem", color: "#fff" }}>
                    {computerName}
                  </strong>
                  <span
                    style={{
                      color: "var(--muted)",
                      fontSize: "0.82rem",
                    }}
                  >
                    Connected
                  </span>
                </div>

                {/* Parsec-Style Per-Person Input & Kick Icons */}
                <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                  <button
                    aria-label="Controller"
                    data-testid={`toggle-perm-gamepad-${session.sessionId}`}
                    onClick={() =>
                      void toggleSessionPerm(
                        session.sessionId,
                        session.hostId,
                        "gamepad",
                      )
                    }
                    style={iconBtnStyle(perms.gamepad)}
                    title={`Controller: ${perms.gamepad ? "On" : "Off"}`}
                    type="button"
                  >
                    <span
                      className="material-symbols-outlined"
                      style={{ fontSize: "1.1rem" }}
                    >
                      sports_esports
                    </span>
                  </button>

                  <button
                    aria-label="Keyboard"
                    data-testid={`toggle-perm-keyboard-${session.sessionId}`}
                    onClick={() =>
                      void toggleSessionPerm(
                        session.sessionId,
                        session.hostId,
                        "keyboard",
                      )
                    }
                    style={iconBtnStyle(perms.keyboard)}
                    title={`Keyboard: ${perms.keyboard ? "On" : "Off"}`}
                    type="button"
                  >
                    <span
                      className="material-symbols-outlined"
                      style={{ fontSize: "1.1rem" }}
                    >
                      keyboard
                    </span>
                  </button>

                  <button
                    aria-label="Mouse"
                    data-testid={`toggle-perm-mouse-${session.sessionId}`}
                    onClick={() =>
                      void toggleSessionPerm(
                        session.sessionId,
                        session.hostId,
                        "mouse",
                      )
                    }
                    style={iconBtnStyle(perms.mouse)}
                    title={`Mouse: ${perms.mouse ? "On" : "Off"}`}
                    type="button"
                  >
                    <span
                      className="material-symbols-outlined"
                      style={{ fontSize: "1.1rem" }}
                    >
                      mouse
                    </span>
                  </button>

                  {(onCancelSession || onKickSession) ? (
                    <button
                      aria-label="Disconnect"
                      data-testid="disconnect-session-button"
                      onClick={() => {
                        if (onCancelSession) {
                          void onCancelSession(session.sessionId);
                        } else if (onKickSession) {
                          void onKickSession(
                            session.sessionId,
                            "disconnected by user",
                          );
                        }
                      }}
                      style={iconBtnStyle(false, true)}
                      title="Kick / Disconnect"
                      type="button"
                    >
                      <span
                        className="material-symbols-outlined"
                        style={{ fontSize: "1.1rem" }}
                      >
                        link_off
                      </span>
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
          Waiting for <strong>{resolveComputerName(connect.hostId)}</strong>…
          <button
            className="secondary-button"
            onClick={() => cancelPendingPair(connect.hostId)}
            style={{ marginLeft: 8 }}
            type="button"
          >
            Cancel
          </button>
        </p>
      ) : null}
      {connect.kind === "error" ? (
        <p className="state state--error">Connection failed: {connect.message}</p>
      ) : null}

      {/* 3. THIS COMPUTER (Compact Host Row) */}
      <div
        className="host-card"
        data-testid="this-computer-card"
        style={{
          padding: "14px 18px",
          display: "flex",
          flexDirection: "column",
          gap: 12,
          flexShrink: 0,
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: 12,
            flexWrap: "wrap",
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
            <strong style={{ fontSize: "1.05rem", color: "#fff" }}>
              {thisComputerName}
            </strong>
            <span style={{ color: "var(--muted)", fontSize: "0.85rem" }}>
              {isSharing ? "Sharing" : "Off"}
            </span>
          </div>

          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            {activeSessions.length > 0 ? (
              <button
                data-testid="kick-all-button"
                onClick={() => void kickAllSessions()}
                style={iconBtnStyle(false, true)}
                title="Kick all guests"
                type="button"
              >
                <span
                  className="material-symbols-outlined"
                  style={{ fontSize: "1.1rem" }}
                >
                  group_off
                </span>
              </button>
            ) : null}

            {settings?.hostPeerId ? (
              <button
                data-testid="copy-host-id"
                onClick={() => {
                  if (settings.hostPeerId) {
                    void navigator.clipboard.writeText(settings.hostPeerId);
                    setHostNotice("Copied ID.");
                  }
                }}
                style={iconBtnStyle(false)}
                title="Copy ID"
                type="button"
              >
                <span
                  className="material-symbols-outlined"
                  style={{ fontSize: "1.05rem" }}
                >
                  content_copy
                </span>
              </button>
            ) : null}

            <button
              data-testid="create-share-code"
              onClick={() => void generateShareCode()}
              style={iconBtnStyle(Boolean(shareCode))}
              title="Invite friends"
              type="button"
            >
              <span
                className="material-symbols-outlined"
                style={{ fontSize: "1.1rem" }}
              >
                person_add
              </span>
            </button>

            <button
              className={isSharing ? "danger-button" : "primary-button"}
              data-testid="toggle-host-sharing"
              disabled={hostBusy}
              onClick={() => void toggleHostSharing()}
              style={{ padding: "8px 16px" }}
              type="button"
            >
              <span
                className="material-symbols-outlined"
                style={{ fontSize: "1.05rem" }}
              >
                {isSharing ? "stop_circle" : "cast"}
              </span>
              {hostBusy ? "…" : isSharing ? "Stop" : "Share"}
            </button>
          </div>
        </div>

        {shareCode ? (
          <div
            className="share-code-box"
            data-testid="share-code-display"
            style={{
              marginTop: 0,
              display: "flex",
              flexDirection: "row",
              alignItems: "center",
              justifyContent: "space-between",
              gap: 12,
              padding: "8px 14px",
              borderRadius: 10,
              background: "rgba(0, 0, 0, 0.35)",
              border: "1px solid var(--panel-border)",
            }}
          >
            <code
              className="share-code"
              style={{
                fontSize: "1.15rem",
                fontWeight: 800,
                letterSpacing: "0.14em",
                color: "var(--primary)",
              }}
            >
              {shareCode}
            </code>
            <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
              <button
                onClick={() => {
                  void navigator.clipboard.writeText(shareCode);
                }}
                style={iconBtnStyle(false)}
                title="Copy code"
                type="button"
              >
                <span
                  className="material-symbols-outlined"
                  style={{ fontSize: "1rem" }}
                >
                  content_copy
                </span>
              </button>
              <button
                aria-label="Hide"
                data-testid="hide-share-code"
                onClick={() => setShareCode(null)}
                style={iconBtnStyle(false)}
                title="Hide"
                type="button"
              >
                <span
                  className="material-symbols-outlined"
                  style={{ fontSize: "1.05rem" }}
                >
                  close
                </span>
              </button>
            </div>
          </div>
        ) : null}
      </div>

      {/* 4. OTHER COMPUTERS (Search / Quick Connect + Scrollable Parsec Access Rows) */}
      <div
        className="host-card"
        style={{
          display: "flex",
          flexDirection: "row",
          gap: 10,
          alignItems: "center",
          padding: "10px 14px",
          flexShrink: 0,
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
          placeholder="Search computers, or enter code / ID…"
          style={{ flex: 1, minWidth: 200 }}
          value={quickInput}
        />
        {quickInput ? (
          <button
            aria-label="Clear search"
            data-testid="clear-search-button"
            onClick={() => setQuickInput("")}
            style={iconBtnStyle(false)}
            title="Clear"
            type="button"
          >
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1.05rem" }}
            >
              close
            </span>
          </button>
        ) : null}
        <button
          className="connect-button"
          data-testid="quick-connect-button"
          disabled={!quickInput.trim() || isBusyFor(quickInput.trim())}
          onClick={() => void handleQuickConnect()}
          style={{ padding: "8px 18px" }}
          type="button"
        >
          Connect
        </button>
      </div>

      {filtered.known.length === 0 && filtered.discovered.length === 0 ? (
        <div
          className="empty-state"
          data-testid="empty-computers-state"
          style={{ padding: "24px 16px", flexShrink: 0 }}
        >
          <p className="empty-state__title" style={{ margin: 0 }}>
            {totalCount > 0 ? "No matching computers" : "No other computers"}
          </p>
        </div>
      ) : (
        <div
          data-testid="computers-grid"
          style={{
            display: "flex",
            flexDirection: "column",
            gap: 8,
            flex: 1,
            minHeight: 0,
            overflowY: "auto",
            paddingRight: 4,
          }}
        >
          {filtered.known.map((host) => {
            const perms = getPermsForPeer(host.hostPeerId);
            return (
              <article
                className="host-card"
                key={host.hostPeerId}
                style={{
                  padding: "10px 16px",
                  display: "flex",
                  flexDirection: "row",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: 12,
                  flexShrink: 0,
                }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
                  <strong style={{ fontSize: "0.95rem", color: "#fff" }}>
                    {host.displayName ?? "Paired Computer"}
                  </strong>
                  <span style={{ color: "var(--muted)", fontSize: "0.82rem" }}>
                    Saved
                  </span>
                </div>

                {/* Parsec-Style Persistent Per-Friend Access Control Icons + Connect */}
                <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                  <button
                    aria-label="Controller permission"
                    data-testid={`peer-perm-gamepad-${host.hostPeerId}`}
                    onClick={() =>
                      void togglePeerPerm(host.hostPeerId, "gamepad")
                    }
                    style={iconBtnStyle(perms.gamepad)}
                    title={`Controller: ${perms.gamepad ? "Allowed" : "Blocked"}`}
                    type="button"
                  >
                    <span
                      className="material-symbols-outlined"
                      style={{ fontSize: "1.05rem" }}
                    >
                      sports_esports
                    </span>
                  </button>

                  <button
                    aria-label="Keyboard permission"
                    data-testid={`peer-perm-keyboard-${host.hostPeerId}`}
                    onClick={() =>
                      void togglePeerPerm(host.hostPeerId, "keyboard")
                    }
                    style={iconBtnStyle(perms.keyboard)}
                    title={`Keyboard: ${perms.keyboard ? "Allowed" : "Blocked"}`}
                    type="button"
                  >
                    <span
                      className="material-symbols-outlined"
                      style={{ fontSize: "1.05rem" }}
                    >
                      keyboard
                    </span>
                  </button>

                  <button
                    aria-label="Mouse permission"
                    data-testid={`peer-perm-mouse-${host.hostPeerId}`}
                    onClick={() =>
                      void togglePeerPerm(host.hostPeerId, "mouse")
                    }
                    style={iconBtnStyle(perms.mouse)}
                    title={`Mouse: ${perms.mouse ? "Allowed" : "Blocked"}`}
                    type="button"
                  >
                    <span
                      className="material-symbols-outlined"
                      style={{ fontSize: "1.05rem" }}
                    >
                      mouse
                    </span>
                  </button>

                  <button
                    aria-label="Connect without approval"
                    data-testid={`peer-perm-auto-${host.hostPeerId}`}
                    onClick={() =>
                      void togglePeerPerm(host.hostPeerId, "autoApprove")
                    }
                    style={iconBtnStyle(perms.autoApprove)}
                    title={`Connect without approval: ${perms.autoApprove ? "On" : "Off"}`}
                    type="button"
                  >
                    <span
                      className="material-symbols-outlined"
                      style={{ fontSize: "1.05rem" }}
                    >
                      lock_open
                    </span>
                  </button>

                  <button
                    aria-label="Remove"
                    data-testid={`remove-known-${host.hostPeerId}`}
                    onClick={() => void removeSavedHost(host.hostPeerId)}
                    style={iconBtnStyle(false, true)}
                    title="Remove computer"
                    type="button"
                  >
                    <span
                      className="material-symbols-outlined"
                      style={{ fontSize: "1.05rem" }}
                    >
                      person_remove
                    </span>
                  </button>

                  <button
                    className="connect-button"
                    data-testid={`connect-known-${host.hostPeerId}`}
                    disabled={isBusyFor(host.hostPeerId)}
                    onClick={() => void handleConnect(host.hostPeerId, false)}
                    style={{ padding: "7px 16px", marginLeft: 4 }}
                    type="button"
                  >
                    {connect.kind === "launching" &&
                    connect.hostId === host.hostPeerId
                      ? "Connecting…"
                      : "Connect"}
                  </button>
                </div>
              </article>
            );
          })}

          {filtered.discovered.map((host) => {
            const waiting = isBusyFor(host.peerId);
            return (
              <article
                className="host-card host-card--discovered"
                key={host.peerId}
                style={{
                  padding: "10px 16px",
                  display: "flex",
                  flexDirection: "row",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: 12,
                  flexShrink: 0,
                }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
                  <strong style={{ fontSize: "0.95rem", color: "#fff" }}>
                    {host.deviceName || "Remote Computer"}
                  </strong>
                  <span style={{ color: "var(--muted)", fontSize: "0.82rem" }}>
                    Online
                  </span>
                </div>
                <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                  {waiting ? (
                    <button
                      onClick={() => cancelPendingPair(host.peerId)}
                      style={iconBtnStyle(false, true)}
                      title="Cancel"
                      type="button"
                    >
                      <span
                        className="material-symbols-outlined"
                        style={{ fontSize: "1.05rem" }}
                      >
                        close
                      </span>
                    </button>
                  ) : (
                    <button
                      className="connect-button"
                      data-testid={`pair-connect-${host.peerId}`}
                      disabled={isBusyFor(host.peerId)}
                      onClick={() => void handleConnect(host.peerId, true)}
                      style={{ padding: "7px 16px" }}
                      type="button"
                    >
                      Connect
                    </button>
                  )}
                </div>
              </article>
            );
          })}
        </div>
      )}
    </div>
  );
}
