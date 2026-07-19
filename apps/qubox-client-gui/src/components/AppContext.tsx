import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type { ReactNode } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import {
  isPermissionGranted,
  requestPermission,
} from "@tauri-apps/plugin-notification";

export type KnownHost = {
  hostPeerId: string;
  displayName: string | null;
};

export type DiscoveredHost = {
  peerId: string;
  deviceName: string;
  transports: string[];
  /** Unix millis when this row was discovered. Used for TTL. */
  discoveredAt: number;
};

export type ActiveSession = {
  sessionId: string;
  hostId: string;
  pid: number | null;
  startedAt: number;
};

export type RecentSession = {
  sessionId: string;
  hostId: string;
  pid: number | null;
  startedAt: number;
  endedAt: number;
  reason: string;
  stderrTail: string[];
};

export type Settings = {
  signalingServer: string | null;
  accountsUrl: string | null;
  cloudMode: boolean;
  autoApprovePairing: boolean;
  bitrateKbps: number | null;
  fpsCap: number | null;
  decoderBackend: string | null;
  micEnabled: boolean;
  clipboardSync: string | null;
  statsOverlay: boolean;
  autoStartHost: boolean;
};

export type SessionTelemetry =
  | {
      kind: "frame_decoded";
      frameId: number;
      bytes: number;
      keyframe: boolean;
    }
  | { kind: "frame_rendered"; rendered: number; skipped: number }
  | { kind: "control"; msg: unknown }
  | { kind: "session_ended"; reason: string }
  | { kind: "raw"; raw: string };

export type PairingRequest = {
  hostId: string;
  requestId: string;
  receivedAt: number;
};

export type StderrLine = {
  sessionId: string;
  line: string;
  level: "info" | "warn" | "error";
  receivedAt: number;
};

/** Host-side pairing queue item (snake_case JSON from the daemon broker). */
export type HostPendingPairing = {
  request_id: string;
  client_peer_id: string;
  client_device_id: string;
  client_name: string;
  client_label: string;
  received_at_unix_ms: number;
};

type AppContextValue = {
  knownHosts: KnownHost[];
  discoveredHosts: DiscoveredHost[];
  activeSessions: ActiveSession[];
  recentSessions: RecentSession[];
  settings: Settings | null;
  hostRunning: boolean | null;
  conflictCount: number;
  telemetryBySession: Record<string, SessionTelemetry[]>;
  stderrBySession: Record<string, StderrLine[]>;
  pendingPairings: PairingRequest[];
  hostPendingPairings: HostPendingPairing[];
  lanIp: string | null;
  notificationsAllowed: boolean;
  setKnownHosts: (hosts: KnownHost[]) => void;
  setDiscoveredHosts: (hosts: DiscoveredHost[]) => void;
  setActiveSessions: (sessions: ActiveSession[]) => void;
  setSettings: (settings: Settings | null) => void;
  setConflictCount: (count: number) => void;
  refreshConflictCount: () => Promise<void>;
  refreshRecentSessions: () => Promise<void>;
  refreshHostStatus: () => Promise<void>;
  refreshKnownHosts: () => Promise<void>;
  ensureNotifications: () => Promise<boolean>;
  pushTelemetry: (sessionId: string, event: SessionTelemetry) => void;
  pushStderr: (line: StderrLine) => void;
  pushPairingRequest: (request: PairingRequest) => void;
  removePairingRequest: (requestId: string) => void;
  removeHostPendingPairing: (requestId: string) => void;
};

const AppContext = createContext<AppContextValue | null>(null);

const TELEMETRY_BUFFER = 256;
const STDERR_BUFFER = 256;
/** Re-flush telemetry/stderr batches at ~60Hz so React renders never
 *  exceed a frame rate, even when the daemon fires faster. */
const FLUSH_INTERVAL_MS = 16;
const DISCOVERY_TTL_MS = 4 * 60 * 1000;

type PendingTelemetry = Map<string, SessionTelemetry[]>;
type PendingStderr = Map<string, StderrLine[]>;

export function AppProvider({ children }: { children: ReactNode }) {
  const [knownHosts, setKnownHosts] = useState<KnownHost[]>([]);
  const [discoveredHosts, setDiscoveredHosts] = useState<DiscoveredHost[]>([]);
  const [activeSessions, setActiveSessions] = useState<ActiveSession[]>([]);
  const [recentSessions, setRecentSessions] = useState<RecentSession[]>([]);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [hostRunning, setHostRunning] = useState<boolean | null>(null);
  const [conflictCount, setConflictCount] = useState(0);
  const [telemetryBySession, setTelemetryBySession] = useState<
    Record<string, SessionTelemetry[]>
  >({});
  const [stderrBySession, setStderrBySession] = useState<
    Record<string, StderrLine[]>
  >({});
  const [pendingPairings, setPendingPairings] = useState<PairingRequest[]>([]);
  const [hostPendingPairings, setHostPendingPairings] = useState<
    HostPendingPairing[]
  >([]);
  const [lanIp, setLanIp] = useState<string | null>(null);
  const [notificationsAllowed, setNotificationsAllowed] = useState(false);

  // Throttled buffers: writes go to mutable refs, React state is flushed
  // on a single 16ms timer. Stops the 60fps telemetry path from blowing
  // the React diff budget.
  const pendingTelemetry = useRef<PendingTelemetry>(new Map());
  const pendingStderr = useRef<PendingStderr>(new Map());
  const flushTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const notifiedHostPairings = useRef<Set<string>>(new Set());

  const scheduleFlush = useCallback(() => {
    if (flushTimer.current !== null) return;
    flushTimer.current = setTimeout(() => {
      flushTimer.current = null;
      const t = pendingTelemetry.current;
      const s = pendingStderr.current;
      pendingTelemetry.current = new Map();
      pendingStderr.current = new Map();
      if (t.size > 0) {
        setTelemetryBySession((prev) => {
          const next = { ...prev };
          for (const [sessionId, events] of t) {
            const merged = (next[sessionId] ?? []).concat(events);
            next[sessionId] = merged.slice(-TELEMETRY_BUFFER);
          }
          return next;
        });
      }
      if (s.size > 0) {
        setStderrBySession((prev) => {
          const next = { ...prev };
          for (const [sessionId, lines] of s) {
            const merged = (next[sessionId] ?? []).concat(lines);
            next[sessionId] = merged.slice(-STDERR_BUFFER);
          }
          return next;
        });
      }
    }, FLUSH_INTERVAL_MS);
  }, []);

  useEffect(() => {
    const initSettings = async () => {
      try {
        const loaded = await invoke<Settings>("get_settings");
        setSettings(loaded);
        if (loaded.autoStartHost) {
          await invoke("start_host_agent", {});
        }
      } catch (e) {
        console.error("Failed to load settings or auto start host", e);
      }
    };
    void initSettings();

    const initLan = async () => {
      try {
        const ip = await invoke<string>("detect_lan_ipv4");
        setLanIp(ip || null);
      } catch (e) {
        console.error("Failed to detect LAN IP", e);
      }
    };
    void initLan();

    const initNotifications = async () => {
      try {
        const granted = await isPermissionGranted();
        if (granted) {
          setNotificationsAllowed(true);
          return;
        }
        const result = await requestPermission();
        setNotificationsAllowed(result === "granted");
      } catch (e) {
        console.error("notification permission check failed", e);
      }
    };
    void initNotifications();

    const initHosts = async () => {
      try {
        const hosts = await invoke<KnownHost[]>("get_known_hosts");
        setKnownHosts(Array.isArray(hosts) ? hosts : []);
      } catch (e) {
        console.error("Failed to load known hosts", e);
      }
    };
    void initHosts();
  }, []);

  const refreshConflictCount = useCallback(async () => {
    try {
      const conflicts = await invoke<unknown[]>("sync_list_conflicts");
      setConflictCount(Array.isArray(conflicts) ? conflicts.length : 0);
    } catch {
      setConflictCount(0);
    }
  }, []);

  const refreshRecentSessions = useCallback(async () => {
    try {
      const list = await invoke<RecentSession[]>("list_recent_sessions");
      setRecentSessions(Array.isArray(list) ? list : []);
    } catch {
      setRecentSessions([]);
    }
  }, []);

  const refreshHostStatus = useCallback(async () => {
    try {
      const running = await invoke<boolean>("get_host_status");
      setHostRunning(running);
    } catch {
      setHostRunning(null);
    }
  }, []);

  const refreshKnownHosts = useCallback(async () => {
    try {
      const hosts = await invoke<KnownHost[]>("get_known_hosts");
      setKnownHosts(Array.isArray(hosts) ? hosts : []);
    } catch {
      // leave existing list intact on transient errors
    }
  }, []);

  const ensureNotifications = useCallback(async () => {
    try {
      const granted = await isPermissionGranted();
      if (granted) {
        setNotificationsAllowed(true);
        return true;
      }
      const result = await requestPermission();
      const ok = result === "granted";
      setNotificationsAllowed(ok);
      return ok;
    } catch {
      return false;
    }
  }, []);

  useEffect(() => {
    void refreshConflictCount();
    void refreshRecentSessions();
    void refreshHostStatus();
  }, [refreshConflictCount, refreshRecentSessions, refreshHostStatus]);

  // TTL sweep for discovered LAN hosts: drop rows older than DISCOVERY_TTL_MS.
  useEffect(() => {
    const t = setInterval(() => {
      setDiscoveredHosts((prev) => {
        const cutoff = Date.now() - DISCOVERY_TTL_MS;
        const next = prev.filter((host) => host.discoveredAt >= cutoff);
        return next.length === prev.length ? prev : next;
      });
    }, 30_000);
    return () => clearInterval(t);
  }, []);

  useEffect(() => {
    const unlistens: UnlistenFn[] = [];

    const registerListeners = async () => {
      const unlistenTelemetry = await listen<{
        sessionId: string;
        event: { op: string } & Record<string, unknown>;
      }>("session://telemetry", (event) => {
        const { sessionId, event: payload } = event.payload;
        const mapped = mapTelemetry(payload);
        const bucket = pendingTelemetry.current.get(sessionId) ?? [];
        bucket.push(mapped);
        pendingTelemetry.current.set(sessionId, bucket);
        scheduleFlush();
      });
      unlistens.push(unlistenTelemetry);

      const unlistenDiscovered = await listen<{
        sessionId: string;
        event: { op: string } & Record<string, unknown>;
      }>("session://host-discovered", (event) => {
        const { event: payload } = event.payload;
        if (payload.op !== "host_discovered") {
          return;
        }
        const peerId = String(payload.peer_id ?? "");
        const deviceName = String(payload.device_name ?? "");
        const transports = Array.isArray(payload.transports)
          ? payload.transports.map((t) => String(t))
          : [];
        if (!peerId) {
          return;
        }
        setDiscoveredHosts((prev) => {
          const without = prev.filter((h) => h.peerId !== peerId);
          return [
            ...without,
            { peerId, deviceName, transports, discoveredAt: Date.now() },
          ];
        });
      });
      unlistens.push(unlistenDiscovered);

      const unlistenPairing = await listen<{
        sessionId: string;
        event: { op: string } & Record<string, unknown>;
      }>("session://pairing-requested", (event) => {
        const { event: payload } = event.payload;
        if (payload.op !== "pairing_requested") {
          return;
        }
        const request: PairingRequest = {
          hostId: String(payload.host_id ?? ""),
          requestId: String(payload.request_id ?? ""),
          receivedAt: Date.now(),
        };
        if (!request.hostId) {
          return;
        }
        setPendingPairings((prev) => {
          if (prev.some((p) => p.requestId === request.requestId)) {
            return prev;
          }
          return [...prev, request];
        });
      });
      unlistens.push(unlistenPairing);

      const unlistenStderr = await listen<StderrLine>(
        "session://stderr",
        (event) => {
          const { sessionId, line, level, receivedAt } = event.payload;
          const bucket = pendingStderr.current.get(sessionId) ?? [];
          bucket.push({
            sessionId,
            line,
            level,
            receivedAt: receivedAt ?? Date.now(),
          });
          pendingStderr.current.set(sessionId, bucket);
          scheduleFlush();
        },
      );
      unlistens.push(unlistenStderr);

      const unlistenEnded = await listen<{ sessionId: string; reason: string }>(
        "session://ended",
        (event) => {
          const sessionId = event.payload.sessionId;
          setActiveSessions((prev) =>
            prev.filter((s) => s.sessionId !== sessionId),
          );
          setTelemetryBySession((prev) => {
            const next = { ...prev };
            delete next[sessionId];
            return next;
          });
          setStderrBySession((prev) => {
            const next = { ...prev };
            delete next[sessionId];
            return next;
          });
          void refreshRecentSessions();
        },
      );
      unlistens.push(unlistenEnded);

      const unlistenStarted = await listen<{
        sessionId: string;
        hostId: string;
      }>("session://started", (event) => {
        setActiveSessions((prev) => {
          if (prev.some((s) => s.sessionId === event.payload.sessionId)) {
            return prev;
          }
          return [
            ...prev,
            {
              sessionId: event.payload.sessionId,
              hostId: event.payload.hostId,
              pid: null,
              startedAt: Date.now(),
            },
          ];
        });
      });
      unlistens.push(unlistenStarted);

      const unlistenDaemon = await listen<{
        event?: unknown;
        host?: string;
      }>("daemon://state-changed", (event) => {
        const payload = event.payload;
        if (payload.host === "starting") {
          setHostRunning(true);
          return;
        }
        if (payload.host === "stopped") {
          setHostRunning(false);
          return;
        }
        if (!payload.event || typeof payload.event !== "object") {
          return;
        }
        const daemonEvent = payload.event as {
          HostStateChanged?: { running?: boolean };
          SyncConflict?: unknown;
          SyncJobUpdated?: unknown;
        };
        if (daemonEvent.SyncConflict || daemonEvent.SyncJobUpdated) {
          void refreshConflictCount();
        }
        if (typeof daemonEvent.HostStateChanged?.running === "boolean") {
          setHostRunning(daemonEvent.HostStateChanged.running);
        }
      });
      unlistens.push(unlistenDaemon);

      // Unified pairing broker: the Rust bridge polls the host-agent
      // HTTP endpoint on a 3s cadence and pushes the full snapshot here.
      // The frontend no longer runs its own setInterval.
      const unlistenPairingBridge = await listen<{
        pending: HostPendingPairing[];
      }>("daemon://pairing-updated", (event) => {
        const list = Array.isArray(event.payload?.pending)
          ? event.payload.pending
          : [];
        setHostPendingPairings(list);
        if (notificationsAllowed) {
          for (const req of list) {
            if (!notifiedHostPairings.current.has(req.request_id)) {
              notifiedHostPairings.current.add(req.request_id);
              void invoke("notify_user", {
                title: "Qubox pairing request",
                body: `${req.client_name || req.client_label || "A client"} wants to pair`,
              }).catch(() => {});
            }
          }
        }
      });
      unlistens.push(unlistenPairingBridge);
    };

    void registerListeners();

    return () => {
      for (const unlisten of unlistens) {
        unlisten();
      }
      if (flushTimer.current !== null) {
        clearTimeout(flushTimer.current);
      }
    };
  }, [
    notificationsAllowed,
    refreshConflictCount,
    refreshRecentSessions,
    scheduleFlush,
  ]);

  const value = useMemo<AppContextValue>(
    () => ({
      knownHosts,
      discoveredHosts,
      activeSessions,
      recentSessions,
      settings,
      hostRunning,
      conflictCount,
      telemetryBySession,
      stderrBySession,
      pendingPairings,
      hostPendingPairings,
      lanIp,
      notificationsAllowed,
      setKnownHosts,
      setDiscoveredHosts,
      setActiveSessions,
      setSettings,
      setConflictCount,
      refreshConflictCount,
      refreshRecentSessions,
      refreshHostStatus,
      refreshKnownHosts,
      ensureNotifications,
      pushTelemetry: (sessionId, event) => {
        const bucket = pendingTelemetry.current.get(sessionId) ?? [];
        bucket.push(event);
        pendingTelemetry.current.set(sessionId, bucket);
        scheduleFlush();
      },
      pushStderr: (line) => {
        const bucket = pendingStderr.current.get(line.sessionId) ?? [];
        bucket.push(line);
        pendingStderr.current.set(line.sessionId, bucket);
        scheduleFlush();
      },
      pushPairingRequest: (request) => {
        setPendingPairings((prev) =>
          prev.some((p) => p.requestId === request.requestId)
            ? prev
            : [...prev, request],
        );
      },
      removePairingRequest: (requestId) => {
        setPendingPairings((prev) =>
          prev.filter((p) => p.requestId !== requestId),
        );
      },
      removeHostPendingPairing: (requestId) => {
        setHostPendingPairings((prev) =>
          prev.filter((p) => p.request_id !== requestId),
        );
        notifiedHostPairings.current.delete(requestId);
      },
    }),
    [
      knownHosts,
      discoveredHosts,
      activeSessions,
      recentSessions,
      settings,
      hostRunning,
      conflictCount,
      telemetryBySession,
      stderrBySession,
      pendingPairings,
      hostPendingPairings,
      lanIp,
      notificationsAllowed,
      refreshConflictCount,
      refreshRecentSessions,
      refreshHostStatus,
      refreshKnownHosts,
      ensureNotifications,
      scheduleFlush,
    ],
  );

  return <AppContext.Provider value={value}>{children}</AppContext.Provider>;
}

export function useApp(): AppContextValue {
  const ctx = useContext(AppContext);
  if (!ctx) {
    throw new Error("useApp must be used inside <AppProvider>");
  }
  return ctx;
}

function mapTelemetry(
  payload: { op: string } & Record<string, unknown>,
): SessionTelemetry {
  switch (payload.op) {
    case "frame_decoded":
      return {
        kind: "frame_decoded",
        frameId: Number(payload.frame_id ?? 0),
        bytes: Number(payload.bytes ?? 0),
        keyframe: Boolean(payload.keyframe),
      };
    case "frame_rendered":
      return {
        kind: "frame_rendered",
        rendered: Number(payload.rendered ?? 0),
        skipped: Number(payload.skipped ?? 0),
      };
    case "control":
      return { kind: "control", msg: payload.msg };
    case "session_ended":
      return { kind: "session_ended", reason: String(payload.reason ?? "") };
    default:
      return { kind: "raw", raw: JSON.stringify(payload) };
  }
}
