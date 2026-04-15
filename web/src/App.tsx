import { Menu } from "lucide-react";
import { useEffect, useState } from "react";
import { useStore } from "./store";
import { PairingGate } from "./components/PairingGate";
import { Sidebar } from "./components/Sidebar";
import { StatusBar } from "./components/StatusBar";
import { EmptyState } from "./components/EmptyState";
import { TerminalView } from "./components/Terminal";
import { ControlBar } from "./components/ControlBar";
import { MobileKeyRow } from "./components/MobileKeyRow";

export function App() {
  const init = useStore((s) => s.init);
  const status = useStore((s) => s.status);
  const snapshot = useStore((s) => s.snapshot);
  const activeSessionId = useStore((s) => s.activeSessionId);
  const closeActiveTab = useStore((s) => s.closeActiveTab);
  const [drawerOpen, setDrawerOpen] = useState(false);

  useEffect(() => {
    init();
  }, [init]);

  if (status.kind === "unauthorized") {
    return <PairingGate />;
  }

  // Resolve the session object from runtime state, but use `activeSessionId`
  // (the stable string) for actually deciding whether to mount the terminal.
  // If we keyed on the resolved object we'd unmount/remount the xterm every
  // time the host briefly omits the session from runtimeState during a
  // delta processing tick.
  const activeSession = activeSessionId
    ? snapshot?.runtimeState?.sessions?.[activeSessionId]
    : null;

  return (
    <div className="h-dvh flex flex-col bg-zinc-900 text-zinc-100 overflow-hidden">
      <header className="md:hidden flex items-center gap-2 h-11 px-3 bg-zinc-800 border-b border-zinc-700 shrink-0">
        <button
          type="button"
          className="p-1.5 rounded hover:bg-zinc-700"
          onClick={() => setDrawerOpen((v) => !v)}
          aria-label="Toggle sidebar"
        >
          <Menu className="size-4" />
        </button>
        <span className="text-sm font-semibold">DevManager</span>
      </header>

      <div className="flex flex-1 min-h-0">
        {/* Desktop sidebar (always visible md+) */}
        <div className="hidden md:flex">
          <Sidebar />
        </div>

        {/* Mobile drawer */}
        {drawerOpen && (
          <>
            <div
              className="fixed inset-0 bg-black/60 z-30 md:hidden"
              onClick={() => setDrawerOpen(false)}
            />
            <div className="fixed top-11 left-0 bottom-0 z-40 md:hidden">
              <Sidebar onItemPicked={() => setDrawerOpen(false)} />
            </div>
          </>
        )}

        <main className="flex-1 flex flex-col min-w-0 min-h-0 bg-zinc-900">
          {!snapshot && (
            <div className="flex-1 flex items-center justify-center text-xs text-zinc-500">
              Waiting for host snapshot…
            </div>
          )}

          {snapshot && activeSessionId && (
            <>
              <ControlBar
                session={
                  activeSession ?? {
                    session_id: activeSessionId,
                    pid: null,
                    status: "Starting",
                    session_kind: null,
                    command_id: null,
                    project_id: null,
                    tab_id: null,
                    exit_code: null,
                    title: null,
                    dimensions: { cols: 100, rows: 30, cell_width: 10, cell_height: 20 },
                  }
                }
                onClose={closeActiveTab}
              />
              <TerminalView sessionId={activeSessionId} />
              <MobileKeyRow sessionId={activeSessionId} />
            </>
          )}

          {snapshot && !activeSessionId && <EmptyState />}
        </main>
      </div>

      <StatusBar />
    </div>
  );
}
