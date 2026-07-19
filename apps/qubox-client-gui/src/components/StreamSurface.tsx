/**
 * StreamSurface — terminal-style panel that streams live `session://stderr`
 * lines from the active client-cli subprocess so the user can see the
 * process is doing real work even though the actual video renders in a
 * separate `qubox-client-cli` window.
 */

import { useEffect, useRef, useState } from "react";
import { useApp } from "./AppContext";

type StreamSurfaceProps = {
  sessionId?: string;
  streamCount?: number;
  embedded?: boolean;
};

const TAIL_LIMIT = 32;

export function StreamSurface({
  sessionId,
  streamCount = 1,
  embedded = false,
}: StreamSurfaceProps) {
  const { stderrBySession } = useApp();
  const lines = (sessionId ? (stderrBySession[sessionId] ?? []) : []).slice(
    -TAIL_LIMIT,
  );
  const [autoScroll, setAutoScroll] = useState(true);
  const [now, setNow] = useState(() => Date.now());
  const containerRef = useRef<HTMLPreElement | null>(null);

  // Heartbeat tick — proves the subprocess is alive even when stderr is
  // silent for long stretches.
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, []);

  useEffect(() => {
    if (autoScroll && containerRef.current) {
      containerRef.current.scrollTop = containerRef.current.scrollHeight;
    }
  }, [lines.length, autoScroll]);

  if (!embedded) {
    return (
      <div
        className="stream-surface stream-surface--external"
        data-testid="stream-surface"
      >
        <div className="stream-surface__header">
          <span className="stream-surface__chip">
            <span
              className="material-symbols-outlined"
              style={{ fontSize: "1rem" }}
            >
              terminal
            </span>
            CLI subprocess monitor
          </span>
          <span className="stream-surface__meta">
            heartbeat {new Date(now).toLocaleTimeString()}
            {sessionId ? ` · session ${sessionId.slice(0, 8)}` : ""}
          </span>
        </div>
        <pre
          className="stream-surface__terminal"
          data-testid="stream-terminal"
          ref={containerRef}
          onScroll={(event) => {
            const el = event.currentTarget;
            const atBottom =
              el.scrollHeight - el.scrollTop - el.clientHeight < 24;
            setAutoScroll(atBottom);
          }}
        >
          {lines.length === 0 ? (
            <span className="log-line log-line--info">
              {sessionId
                ? "Waiting for first output from qubox-client-cli…"
                : "No active session."}
            </span>
          ) : (
            lines.map((entry, idx) => (
              <span
                className={`log-line log-line--${entry.level}`}
                key={`${entry.receivedAt}-${idx}`}
              >
                {entry.line}
                {"\n"}
              </span>
            ))
          )}
        </pre>
        <p className="subtitle">
          {streamCount > 1 ? `Receiving ${streamCount} streams · ` : ""}
          Video still renders in the external <code>qubox-client-cli</code>{" "}
          window.
        </p>
      </div>
    );
  }
  return (
    <div className="stream-surface stream-surface--embedded">
      <div className="stream-surface__placeholder">
        <span>
          Embedded surface ({streamCount} stream{streamCount === 1 ? "" : "s"})
        </span>
        <p className="subtitle">
          Placeholder for future wgpu/WebView embed. Media path remains CLI
          until Tauri texture bridge ships.
        </p>
      </div>
    </div>
  );
}
