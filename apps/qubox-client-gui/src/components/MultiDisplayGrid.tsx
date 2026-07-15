type MultiDisplayGridProps = {
  streamCount: number;
  lastKeyframe: boolean;
  /** Optional per-stream labels (Display 1… when omitted). */
  displayLabels?: string[];
  /** 0-based active tile highlight from telemetry. */
  activeIndex?: number;
};

export function MultiDisplayGrid({
  streamCount,
  lastKeyframe,
  displayLabels,
  activeIndex,
}: MultiDisplayGridProps) {
  const tiles = Math.max(streamCount, 1);
  const cols = Math.min(tiles, 3);
  return (
    <div className="display-grid">
      <div className="display-grid__header">
        <span className="display-grid__title">Multi-display</span>
        <span className="display-grid__meta">
          {streamCount} stream{streamCount === 1 ? "" : "s"} ·{" "}
          {lastKeyframe ? "keyframe ready" : "no keyframe"}
        </span>
      </div>
      <div
        className="display-grid__tiles"
        style={{
          gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))`,
        }}
      >
        {Array.from({ length: tiles }, (_, index) => {
          const label =
            displayLabels?.[index] ?? `Display ${index + 1}`;
          const active = activeIndex === index;
          return (
            <div
              className={
                active
                  ? "display-grid__tile display-grid__tile--active"
                  : "display-grid__tile"
              }
              key={index}
            >
              <span className="display-grid__tile-label">{label}</span>
              <span className="display-grid__tile-state">
                {lastKeyframe ? (active ? "active · ready" : "ready") : "buffering"}
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
