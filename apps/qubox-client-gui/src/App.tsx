import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { AppProvider, useApp } from "./components/AppContext";
import { Sidebar, type View } from "./components/Sidebar";
import { HostList } from "./components/HostList";
import { PairingRequests } from "./components/PairingRequests";
import { SessionView } from "./components/SessionView";
import { SettingsView } from "./components/Settings";
import { FileSyncView } from "./components/FileSync";
import { FirstRun } from "./components/FirstRun";
import { HostModeView } from "./components/HostMode";

function Shell() {
  const [view, setView] = useState<View>("hosts");
  const [ready, setReady] = useState(false);
  const [needsOnboarding, setNeedsOnboarding] = useState(false);
  const [drainHint, setDrainHint] = useState<string | null>(null);
  const {
    pendingPairings,
    activeSessions,
    conflictCount,
    ensureNotifications,
  } = useApp();

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const o = await invoke<{ completed: boolean }>("get_onboarding");
        if (!cancelled) {
          setNeedsOnboarding(!o.completed);
          setReady(true);
        }
      } catch {
        // Daemon offline: still show app; first-run will fail with hint.
        if (!cancelled) {
          setNeedsOnboarding(true);
          setReady(true);
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let un: (() => void) | undefined;
    void listen<{ pending: number }>("filesync://drain-ready", (e) => {
      const n = e.payload?.pending ?? 0;
      if (n > 0) {
        setDrainHint(`${n} File Sync job(s) ready while session is up`);
      }
    }).then((fn) => {
      un = fn;
    });
    return () => {
      un?.();
    };
  }, []);

  // Ask for notification permission as soon as the main app shell loads;
  // the user is going to need it for hidden-window pairing/conflict pings.
  useEffect(() => {
    void ensureNotifications();
  }, [ensureNotifications]);

  const startSession = async (hostId: string) => {
    try {
      await invoke<string>("start_session_subprocess", {
        hostId,
        options: {
          mic: false,
          clipboardSync: "off",
          statsOverlay: true,
        },
      });
      setView("sessions");
    } catch (error) {
      console.error("start_session_subprocess failed", error);
      throw error;
    }
  };

  const pairAndStartSession = async (hostId: string) => {
    await invoke("pair_host", { hostId });
    await startSession(hostId);
  };

  const cancelSession = async (sessionId: string) => {
    try {
      await invoke("cancel_session", { sessionId });
    } catch (error) {
      console.error("cancel_session failed", error);
    }
  };

  const kickSession = async (sessionId: string, reason: string) => {
    try {
      await invoke("kick_session", { sessionId, reason });
    } catch (error) {
      console.error("kick_session failed", error);
    }
  };

  if (!ready) {
    return (
      <main className="shell" data-testid="shell-loading">
        <p className="state">Starting…</p>
      </main>
    );
  }

  if (needsOnboarding) {
    return (
      <main className="shell" data-testid="shell-onboarding">
        <section className="panel" style={{ flex: 1 }}>
          <FirstRun onDone={() => setNeedsOnboarding(false)} />
        </section>
      </main>
    );
  }

  return (
    <main className="shell" data-testid="shell-app">
      <Sidebar
        activeSessionCount={activeSessions.length}
        onChange={setView}
        pendingPairingCount={pendingPairings.length}
        view={view}
        conflictCount={conflictCount}
      />
      <section className="panel">
        {drainHint ? (
          <p className="state" style={{ margin: "12px 20px 0" }}>
            {drainHint}{" "}
            <button
              className="secondary-button"
              onClick={() => {
                setView("sync");
                setDrainHint(null);
              }}
              type="button"
            >
              Open File Sync
            </button>
          </p>
        ) : null}
        {view === "hosts" ? (
          <HostList
            onStartSession={startSession}
            onPairAndStartSession={pairAndStartSession}
          />
        ) : null}
        {view === "host" ? <HostModeView /> : null}
        {view === "pairing" ? <PairingRequests /> : null}
        {view === "sessions" ? (
          <SessionView onCancel={cancelSession} onKick={kickSession} />
        ) : null}
        {view === "sync" ? <FileSyncView /> : null}
        {view === "settings" ? <SettingsView /> : null}
      </section>
    </main>
  );
}

function App() {
  return (
    <AppProvider>
      <Shell />
    </AppProvider>
  );
}

export default App;
