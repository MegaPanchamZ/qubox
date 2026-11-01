import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AppProvider, useApp } from "./components/AppContext";
import { Sidebar, type View } from "./components/Sidebar";
import { HostList } from "./components/HostList";
import { SettingsView } from "./components/Settings";
import { FirstRun } from "./components/FirstRun";

function Shell() {
  const [view, setView] = useState<View>("hosts");
  const [ready, setReady] = useState(false);
  const [needsOnboarding, setNeedsOnboarding] = useState(false);
  const {
    pendingPairings,
    hostPendingPairings,
    activeSessions,
    conflictCount,
    ensureNotifications,
    settings,
    refreshKnownHosts,
    refreshSettings,
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
    void ensureNotifications();
  }, [ensureNotifications]);

  const startSession = async (hostId: string) => {
    try {
      await invoke<string>("start_session_subprocess", {
        hostId,
        options: {
          mic: settings?.micEnabled ?? false,
          clipboardSync: settings?.clipboardSync ?? "off",
          statsOverlay: settings?.statsOverlay ?? true,
        },
      });
      setView("hosts");
    } catch (error) {
      console.error("start_session_subprocess failed", error);
      throw error;
    }
  };

  const pairAndStartSession = async (hostId: string) => {
    await invoke("pair_host", { hostId });
    await refreshKnownHosts();
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
          <FirstRun
            onDone={() => {
              void refreshSettings();
              void refreshKnownHosts();
              setNeedsOnboarding(false);
            }}
          />
        </section>
      </main>
    );
  }

  return (
    <main className="shell" data-testid="shell-app">
      <Sidebar
        activeSessionCount={activeSessions.length}
        onChange={setView}
        pendingPairingCount={pendingPairings.length + hostPendingPairings.length}
        view={view}
        conflictCount={conflictCount}
      />
      <section className="panel">
        {view === "settings" ? (
          <SettingsView />
        ) : (
          <HostList
            onStartSession={startSession}
            onPairAndStartSession={pairAndStartSession}
            onCancelSession={cancelSession}
            onKickSession={kickSession}
          />
        )}
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
