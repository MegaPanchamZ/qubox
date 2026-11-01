export type View =
  | "hosts"
  | "host"
  | "pairing"
  | "sessions"
  | "sync"
  | "settings";

type SidebarProps = {
  view: View;
  onChange: (view: View) => void;
  pendingPairingCount: number;
  activeSessionCount: number;
  conflictCount?: number;
};

const ITEMS: { id: View; label: string; icon: string }[] = [
  { id: "hosts", label: "Computers", icon: "devices" },
  { id: "settings", label: "Settings", icon: "settings" },
];

export function Sidebar({
  view,
  onChange,
  pendingPairingCount,
  activeSessionCount,
  conflictCount = 0,
}: SidebarProps) {
  return (
    <aside className="rail" data-testid="sidebar">
      <div className="rail__brand">
        <span className="rail__logo-text">QUBOX</span>
      </div>
      <nav className="rail__nav">
        {ITEMS.map((item) => {
          const badge =
            item.id === "hosts"
              ? pendingPairingCount + activeSessionCount
              : item.id === "settings"
                ? conflictCount
                : 0;
          const active =
            item.id === "hosts"
              ? view !== "settings"
              : view === "settings";
          return (
            <button
              key={item.id}
              className={`rail__item${active ? " rail__item--active" : ""}`}
              data-testid={`nav-${item.id}`}
              onClick={() => onChange(item.id)}
              type="button"
            >
              <span
                className="material-symbols-outlined rail__icon"
                aria-hidden="true"
              >
                {item.icon}
              </span>
              <span className="rail__label">{item.label}</span>
              {badge > 0 ? <span className="rail__badge">{badge}</span> : null}
            </button>
          );
        })}
      </nav>
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 8,
          justifyContent: "center",
          fontSize: "0.78rem",
          color: "var(--muted)",
          paddingTop: 8,
        }}
      >
        <div className="rail__status" style={{ margin: 0 }} />
        <span>Iroh P2P Ready</span>
      </div>
    </aside>
  );
}
