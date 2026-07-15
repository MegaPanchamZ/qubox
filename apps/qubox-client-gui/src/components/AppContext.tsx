import { createContext, useContext, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type KnownHost = {
  hostPeerId: string;
  displayName: string | null;
};

export type DiscoveredHost = {
  peerId: string;
  deviceName: string;
  transports: string[];
};

export type ActiveSession = {
  sessionId: string;
  hostId: string;
  pid: number | null;
  startedAt: number;
};

export type Settings = {
  signalingServer: string | null;
  autoApprovePairing: boolean;
  bitrateKbps: number | null;
  fpsCap: number | null;
  decoderBackend: string | null;
  micEnabled: boolean;
  clipboardSync: string | null;
  statsOverlay: boolean;
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

type AppContextValue = {
  knownHosts: KnownHost[];
  discoveredHosts: DiscoveredHost[];
  activeSessions: ActiveSession[];
  settings: Settings | null;
  telemetryBySession: Record<string, SessionTelemetry[]>;
  stderrBySession: Record<string, StderrLine[]>;
  pendingPairings: PairingRequest[];
  setKnownHosts: (hosts: KnownHost[]) => void;
  setDiscoveredHosts: (hosts: DiscoveredHost[]) => void;
  setActiveSessions: (sessions: ActiveSession[]) => void;
  setSettings: (settings: Settings | null) => void;
  pushTelemetry: (sessionId: string, event: SessionTelemetry) => void;
  pushStderr: (line: StderrLine) => void;
  pushPairingRequest: (request: PairingRequest) => void;
  removePairingRequest: (requestId: string) => void;
};

const AppContext = createContext<AppContextValue | null>(null);

const TELEMETRY_BUFFER = 256;
const STDERR_BUFFER = 256;

export function AppProvider({ children }: { children: ReactNode }) {
  const [knownHosts, setKnownHosts] = useState<KnownHost[]>([]);
  const [discoveredHosts, setDiscoveredHosts] = useState<DiscoveredHost[]>([]);
  const [activeSessions, setActiveSessions] = useState<ActiveSession[]>([]);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [telemetryBySession, setTelemetryBySession] = useState<
    Record<string, SessionTelemetry[]>
  >({});
  const [stderrBySession, setStderrBySession] = useState<
    Record<string, StderrLine[]>
  >({});
  const [pendingPairings, setPendingPairings] = useState<PairingRequest[]>([]);

  useEffect(() => {
    const unlistens: UnlistenFn[] = [];

    const registerListeners = async () => {
      const unlistenTelemetry = await listen<{
        sessionId: string;
        event: { op: string } & Record<string, unknown>;
      }>("session://telemetry", (event) => {
        const { sessionId, event: payload } = event.payload;
        const mapped = mapTelemetry(payload);
        setTelemetryBySession((prev) => {
          const list = prev[sessionId] ?? [];
          const next = [...list, mapped].slice(-TELEMETRY_BUFFER);
          return { ...prev, [sessionId]: next };
        });
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
          if (prev.some((host) => host.peerId === peerId)) {
            return prev;
          }
          return [...prev, { peerId, deviceName, transports }];
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

      const unlistenStderr = await listen<StderrLine>("session://stderr", (event) => {
        const { sessionId, line, level, receivedAt } = event.payload;
        setStderrBySession((prev) => {
          const list = prev[sessionId] ?? [];
          const next = [
            ...list,
            { sessionId, line, level, receivedAt: receivedAt ?? Date.now() },
          ].slice(-STDERR_BUFFER);
          return { ...prev, [sessionId]: next };
        });
      });
      unlistens.push(unlistenStderr);

      const unlistenEnded = await listen<{ sessionId: string; reason: string }>(
        "session://ended",
        (event) => {
          setActiveSessions((prev) =>
            prev.filter((s) => s.sessionId !== event.payload.sessionId),
          );
        },
      );
      unlistens.push(unlistenEnded);

      const unlistenStarted = await listen<{ sessionId: string; hostId: string }>(
        "session://started",
        (event) => {
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
        },
      );
      unlistens.push(unlistenStarted);

      const unlistenDaemon = await listen<{ event: unknown }>(
        "daemon://state-changed",
        () => {
          // Reserved for future daemon-state mirroring.
        },
      );
      unlistens.push(unlistenDaemon);
    };

    void registerListeners();

    return () => {
      for (const unlisten of unlistens) {
        unlisten();
      }
    };
  }, []);

  const value = useMemo<AppContextValue>(
    () => ({
      knownHosts,
      discoveredHosts,
      activeSessions,
      settings,
      telemetryBySession,
      stderrBySession,
      pendingPairings,
      setKnownHosts,
      setDiscoveredHosts,
      setActiveSessions,
      setSettings,
      pushTelemetry: (sessionId, event) => {
        setTelemetryBySession((prev) => {
          const list = prev[sessionId] ?? [];
          const next = [...list, event].slice(-TELEMETRY_BUFFER);
          return { ...prev, [sessionId]: next };
        });
      },
      pushStderr: (line) => {
        setStderrBySession((prev) => {
          const list = prev[line.sessionId] ?? [];
          const next = [...list, line].slice(-STDERR_BUFFER);
          return { ...prev, [line.sessionId]: next };
        });
      },
      pushPairingRequest: (request) => {
        setPendingPairings((prev) =>
          prev.some((p) => p.requestId === request.requestId)
            ? prev
            : [...prev, request],
        );
      },
      removePairingRequest: (requestId) => {
        setPendingPairings((prev) => prev.filter((p) => p.requestId !== requestId));
      },
    }),
    [
      knownHosts,
      discoveredHosts,
      activeSessions,
      settings,
      telemetryBySession,
      stderrBySession,
      pendingPairings,
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

function mapTelemetry(payload: { op: string } & Record<string, unknown>): SessionTelemetry {
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
