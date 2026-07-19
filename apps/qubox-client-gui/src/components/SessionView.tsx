import { useMemo, useState } from "react";
import { useApp } from "./AppContext";
import type {
  ActiveSession,
  SessionTelemetry,
  StderrLine,
} from "./AppContext";
import { MultiDisplayGrid } from "./MultiDisplayGrid";
import { StreamSurface } from "./StreamSurface";
import { StatsOverlay } from "./StatsOverlay";

type SessionViewProps = {
  onCancel: (sessionId: string) => void;
  onKick: (sessionId: string, reason: string) => void;
};

export function SessionView({ onCancel, onKick }: SessionViewProps) {
  const { activeSessions, recentSessions, telemetryBySession, stderrBySession } =
    useApp();
  const [showRecent, setShowRecent] = useState(false);

  const sorted = useMemo(
    () => [...activeSessions].sort((a, b) => b.startedAt - a.startedAt),
    [activeSessions],
  );

  return (
    <div className="view">
      <header className="view__header">
        <div>
          <p className="eyebrow">Sessions</p>
          <h1>Active sessions</h1>
          <p className="subtitle">
            Each session is a `client-cli start-session` subprocess. Disconnect
            to terminate cleanly.
          </p>
        </div>
        <div className="view__actions">
          <button
            className="secondary-button"
            onClick={() => setShowRecent((s) => !s)}
            type="button"
          >
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>
              history
            </span>
            {showRecent ? "Hide recent" : "Recent crashes"}
            {recentSessions.length > 0 ? ` (${recentSessions.length})` : null}
          </button>
        </div>
      </header>

      {showRecent ? <RecentSessionsPanel recent={recentSessions} /> : null}

      {sorted.length === 0 ? (
        <div className="empty-state">
          <span className="material-symbols-outlined" style={{ fontSize: "2.5rem", color: "var(--muted)", marginBottom: "12px" }}>
            play_disabled
          </span>
          <p className="empty-state__title">No active sessions</p>
          <p className="empty-state__body">
            Connect to a host from the Hosts tab to start a session.
          </p>
        </div>
      ) : (
        <div className="session-list">
          {sorted.map((session) => (
            <SessionRow
              key={session.sessionId}
              session={session}
              telemetry={telemetryBySession[session.sessionId] ?? []}
              stderr={stderrBySession[session.sessionId] ?? []}
              onCancel={onCancel}
              onKick={onKick}
            />
          ))}
        </div>
      )}
    </div>
  );
}

type SessionRowProps = {
  session: ActiveSession;
  telemetry: SessionTelemetry[];
  stderr: StderrLine[];
  onCancel: (sessionId: string) => void;
  onKick: (sessionId: string, reason: string) => void;
};

function SessionRow({ session, telemetry, stderr, onCancel, onKick }: SessionRowProps) {
  const stats = useMemo(() => computeStats(telemetry), [telemetry]);
  const lastLog = stderr[stderr.length - 1];
  const [kickArmed, setKickArmed] = useState(false);

  return (
    <article className="session-card">
      <header className="session-card__header">
        <div>
          <p className="host-card__label">Session</p>
          <h2>{session.sessionId}</h2>
          <p className="host-card__id">host: {session.hostId}</p>
        </div>
        <div className="session-card__actions">
          <button
            className="secondary-button"
            disabled={!kickArmed}
            onClick={() => {
              if (kickArmed) {
                onKick(session.sessionId, "admin_kick");
                setKickArmed(false);
              } else {
                setKickArmed(true);
              }
            }}
            type="button"
          >
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>logout</span>
            {kickArmed ? "Confirm kick" : "Kick"}
          </button>
          <button
            className="danger-button"
            onClick={() => onCancel(session.sessionId)}
            type="button"
          >
            <span className="material-symbols-outlined" style={{ fontSize: "1.1rem" }}>power_settings_new</span>
            Disconnect
          </button>
        </div>
      </header>

      <section className="session-card__grid">
        <StreamSurface
          sessionId={session.sessionId}
          streamCount={Math.max(stats.streamCount, 1)}
          embedded={false}
        />
        <MultiDisplayGrid
          streamCount={Math.max(stats.streamCount, 1)}
          lastKeyframe={stats.lastKeyframe}
          displayLabels={stats.displayLabels}
          activeIndex={stats.activeIndex}
          keyframeResetKey={stats.activeIndex ?? 0}
        />
        <StatsOverlay
          bitrateKbps={stats.bitrateKbps}
          rttMs={stats.rttMs}
          lossPercent={stats.lossPercent}
          jitterMs={stats.jitterMs}
          fps={stats.fps}
          framesDecoded={stats.framesDecoded}
          framesDropped={stats.framesDropped}
        />
      </section>

      <section className="session-card__logs">
        <h3 className="section__title">Process log</h3>
        {stderr.length === 0 ? (
          <p className="state">No log output yet.</p>
        ) : (
          <pre className="log-view">
            {stderr.slice(-32).map((entry, index) => (
              <span className={`log-line log-line--${entry.level}`} key={index}>
                {entry.line}
              </span>
            ))}
            {lastLog ? null : null}
          </pre>
        )}
      </section>
    </article>
  );
}

function RecentSessionsPanel({ recent }: { recent: ReturnType<typeof useApp>["recentSessions"] }) {
  if (recent.length === 0) {
    return (
      <div className="empty-state" style={{ marginBottom: 16 }}>
        <p className="empty-state__title">No recent sessions</p>
        <p className="empty-state__body">
          Last 32 terminated sessions show here with their final log lines so
          you can diagnose a crash.
        </p>
      </div>
    );
  }
  return (
    <section className="settings-field" style={{ marginBottom: 16 }}>
      <span>Recent sessions</span>
      <p className="subtitle">
        Diagnose crashed sessions here — log tails survive after the session
        closes.
      </p>
      <ul className="host-list">
        {recent.map((r) => (
          <li key={r.sessionId} className="host-card">
            <strong>{r.sessionId}</strong> <code>{r.hostId.slice(0, 12)}…</code>
            <div className="subtitle">
              ended {new Date(r.endedAt).toLocaleString()} · reason {r.reason}
            </div>
            {r.stderrTail.length > 0 ? (
              <pre className="log-view">
                {r.stderrTail.map((line, idx) => (
                  <span className="log-line log-line--info" key={idx}>
                    {line}
                  </span>
                ))}
              </pre>
            ) : (
              <p className="state">No log tail.</p>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}

type DerivedStats = {
  bitrateKbps: number;
  rttMs: number;
  lossPercent: number;
  jitterMs: number;
  fps: number;
  framesDecoded: number;
  framesDropped: number;
  streamCount: number;
  lastKeyframe: boolean;
  displayLabels?: string[];
  activeIndex?: number;
};

function computeStats(telemetry: SessionTelemetry[]): DerivedStats {
  const stats: DerivedStats = {
    bitrateKbps: 0,
    rttMs: 0,
    lossPercent: 0,
    jitterMs: 0,
    fps: 0,
    framesDecoded: 0,
    framesDropped: 0,
    streamCount: 0,
    lastKeyframe: false,
    displayLabels: undefined,
    activeIndex: undefined,
  };

  let renderedFrames = 0;

  for (const event of telemetry) {
    if (event.kind === "control") {
      const msg = event.msg as
        | { RateFeedback?: { rtt_ms?: number; loss_x1000?: number; jitter_ms?: number } }
        | { StreamStats?: { frames_decoded?: number; frames_dropped?: number } }
        | { stream_count?: number };
      if ("RateFeedback" in msg && msg.RateFeedback) {
        stats.rttMs = msg.RateFeedback.rtt_ms ?? stats.rttMs;
        const loss = msg.RateFeedback.loss_x1000 ?? 0;
        stats.lossPercent = loss / 10;
        stats.jitterMs = msg.RateFeedback.jitter_ms ?? stats.jitterMs;
      } else if ("StreamStats" in msg && msg.StreamStats) {
        stats.framesDecoded = msg.StreamStats.frames_decoded ?? stats.framesDecoded;
        stats.framesDropped = msg.StreamStats.frames_dropped ?? stats.framesDropped;
      } else if ("stream_count" in msg) {
        stats.streamCount = msg.stream_count ?? stats.streamCount;
      }
      const multi = msg as {
        display_labels?: string[];
        active_display?: number;
        stream_index?: number;
      };
      if (Array.isArray(multi.display_labels) && multi.display_labels.length > 0) {
        stats.displayLabels = multi.display_labels;
        stats.streamCount = Math.max(stats.streamCount, multi.display_labels.length);
      }
      if (typeof multi.active_display === "number") {
        stats.activeIndex = multi.active_display;
      } else if (typeof multi.stream_index === "number") {
        stats.activeIndex = multi.stream_index;
      }
    } else if (event.kind === "frame_decoded") {
      stats.bitrateKbps = Math.max(stats.bitrateKbps, Math.round(event.bytes * 8 / 1000));
      stats.lastKeyframe = event.keyframe;
    } else if (event.kind === "frame_rendered") {
      renderedFrames = event.rendered;
    }
  }

  if (renderedFrames > 0 && telemetry.length > 0) {
    stats.fps = Math.round(renderedFrames / Math.max(telemetry.length / 60, 1));
  }

  return stats;
}
