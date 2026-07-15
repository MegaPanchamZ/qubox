type StatsOverlayProps = {
  bitrateKbps: number;
  rttMs: number;
  lossPercent: number;
  jitterMs: number;
  fps: number;
  framesDecoded: number;
  framesDropped: number;
};

export function StatsOverlay({
  bitrateKbps,
  rttMs,
  lossPercent,
  jitterMs,
  fps,
  framesDecoded,
  framesDropped,
}: StatsOverlayProps) {
  return (
    <div className="stats-overlay">
      <div className="stats-overlay__header">
        <span className="stats-overlay__title">Live stats</span>
        <span className="stats-overlay__meta">from client-cli</span>
      </div>
      <dl className="stats-grid">
        <div className="stats-grid__row">
          <dt>FPS</dt>
          <dd>{fps.toFixed(1)}</dd>
        </div>
        <div className="stats-grid__row">
          <dt>Bitrate</dt>
          <dd>{formatBitrate(bitrateKbps * 1000)}</dd>
        </div>
        <div className="stats-grid__row">
          <dt>RTT</dt>
          <dd>{rttMs} ms</dd>
        </div>
        <div className="stats-grid__row">
          <dt>Loss</dt>
          <dd>{lossPercent.toFixed(2)}%</dd>
        </div>
        <div className="stats-grid__row">
          <dt>Jitter</dt>
          <dd>{jitterMs} ms</dd>
        </div>
        <div className="stats-grid__row">
          <dt>Frames decoded</dt>
          <dd>{framesDecoded}</dd>
        </div>
        <div className="stats-grid__row">
          <dt>Frames dropped</dt>
          <dd>{framesDropped}</dd>
        </div>
      </dl>
    </div>
  );
}

function formatBitrate(bps: number): string {
  if (bps >= 1_000_000) {
    return `${(bps / 1_000_000).toFixed(1)} Mbps`;
  }
  if (bps >= 1_000) {
    return `${(bps / 1_000).toFixed(0)} kbps`;
  }
  return `${bps} bps`;
}
