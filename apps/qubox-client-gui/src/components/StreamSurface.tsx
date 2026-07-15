/**
 * Optional embedded stream surface inside Tauri.
 * Default product path still uses the CLI dual-window viewer; this panel
 * is a control-plane placeholder that can host a future WebView/video texture.
 */

type StreamSurfaceProps = {
  sessionId?: string;
  streamCount?: number;
  embedded?: boolean;
};

export function StreamSurface({
  sessionId,
  streamCount = 1,
  embedded = false,
}: StreamSurfaceProps) {
  if (!embedded) {
    return (
      <div className="stream-surface stream-surface--external">
        <p className="subtitle">
          Video runs in the <code>qubox-client-cli</code> window for this release.
          {sessionId ? ` Session ${sessionId}.` : ""}
        </p>
      </div>
    );
  }
  return (
    <div className="stream-surface stream-surface--embedded">
      <div className="stream-surface__placeholder">
        <span>Embedded surface ({streamCount} stream{streamCount === 1 ? "" : "s"})</span>
        <p className="subtitle">
          Placeholder for future wgpu/WebView embed. Media path remains CLI until
          Tauri texture bridge ships.
        </p>
      </div>
    </div>
  );
}
