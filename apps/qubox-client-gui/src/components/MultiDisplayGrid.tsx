import { useEffect, useState } from "react";

type MultiDisplayGridProps = {
  streamCount: number;
  lastKeyframe: boolean;
  /** Optional per-stream labels (Display 1… when omitted). */
  displayLabels?: string[];
  /** 0-based active tile highlight from telemetry. */
  activeIndex?: number;
  /**
   * Bumped every time the active tile should re-buffer. The grid forces
   * itself into the "buffering" state when this key changes so the
   * stale keyframe from the previous display is never re-used.
   */
  keyframeResetKey?: number | string;
};

export function MultiDisplayGrid({
  streamCount,
  lastKeyframe,
  displayLabels,
  activeIndex,
  keyframeResetKey,
}: MultiDisplayGridProps) {
  const tiles = Math.max(streamCount, 1);
  const cols = Math.min(tiles, 3);
  const [readyForKey, setReadyForKey] = useState(keyframeResetKey);
  const [showReady, setShowReady] = useState(false);

  useEffect(() => {
    if (readyForKey !== keyframeResetKey) {
      setReadyForKey(keyframeResetKey);
      setShowReady(false);
    } else if (lastKeyframe) {
      setShowReady(true);
    }
  }, [keyframeResetKey, lastKeyframe, readyForKey]);

  return (
    <div className="display-grid">
      <div className="display-grid__header">
        <span className="display-grid__title">Multi-display</span>
        <span className="display-grid__meta">
          {streamCount} stream{streamCount === 1 ? "" : "s"} ·{" "}
          {showReady ? "keyframe ready" : "no keyframe"}
        </span>
      </div>
      <div
        className="display-grid__tiles"
        style={{
          gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))`,
        }}
      >
        {Array.from({ length: tiles }, (_, index) => {
          const label = displayLabels?.[index] ?? `Display ${index + 1}`;
          const active = activeIndex === index;
          const stateLabel = showReady
            ? active
              ? "active · ready"
              : "ready"
            : "buffering";
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
              <span className="display-grid__tile-state">{stateLabel}</span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
