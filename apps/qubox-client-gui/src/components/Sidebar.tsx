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
  { id: "hosts", label: "Hosts", icon: "devices" },
  { id: "host", label: "Host mode", icon: "router" },
  { id: "pairing", label: "Pairing", icon: "link" },
  { id: "sessions", label: "Sessions", icon: "play_circle" },
  { id: "sync", label: "File Sync", icon: "swap_horiz" },
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
    <aside className="rail">
      <div className="rail__brand">
        <span className="rail__logo-text">QUBOX</span>
      </div>
      <nav className="rail__nav">
        {ITEMS.map((item) => {
          const badge =
            item.id === "pairing"
              ? pendingPairingCount
              : item.id === "sessions"
                ? activeSessionCount
                : item.id === "sync"
                  ? conflictCount
                  : 0;
          const active = view === item.id;
          return (
            <button
              key={item.id}
              className={`rail__item${active ? " rail__item--active" : ""}`}
              onClick={() => onChange(item.id)}
              type="button"
            >
              <span className="material-symbols-outlined rail__icon" aria-hidden="true">
                {item.icon}
              </span>
              <span className="rail__label">{item.label}</span>
              {badge > 0 ? <span className="rail__badge">{badge}</span> : null}
            </button>
          );
        })}
      </nav>
      <div className="rail__status" />
    </aside>
  );
}
