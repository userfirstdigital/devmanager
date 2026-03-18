import { useEffect } from 'react';
import { Sidebar } from './Sidebar';
import { StatusBar } from './StatusBar';
import { InteractiveTerminal } from '../terminal/InteractiveTerminal';
import { SSHToolbar } from '../ssh/SSHToolbar';
import { useAppStore } from '../../stores/appStore';
import { useProcessStore } from '../../stores/processStore';
import { getTerminalHeaderLabel } from '../../utils/tabTitles';

export function AppLayout() {
  const activeTabId = useAppStore(s => s.activeTabId);
  const openTabs = useAppStore(s => s.openTabs);
  const config = useAppStore(s => s.config);

  const activeTab = openTabs.find(t => t.id === activeTabId);

  // Clear "ready" indicator when user visits a Claude/Codex tab.
  useEffect(() => {
    if ((activeTab?.type === 'claude' || activeTab?.type === 'codex') && activeTab.ptySessionId) {
      useProcessStore.getState().clearUnseenReady(activeTab.ptySessionId);
    }
  }, [activeTab]);

  const handlePtyExit = (sessionId: string) => {
    useProcessStore.getState().setProcessState(sessionId, {
      status: 'stopped',
      pid: null,
    });
  };

  // All tabs with PTY sessions: keep mounted, CSS show/hide so xterm stays alive.
  const ptyTabs = openTabs.filter(t => t.ptySessionId);

  return (
    <div className="flex h-screen bg-zinc-900 text-zinc-100 overflow-hidden">
      <Sidebar />
      <div className="flex flex-col flex-1 min-w-0">
        <div className="flex-1 min-h-0 flex flex-col relative">
          {/* All PTY tabs: always mounted, toggle visibility via CSS. */}
          {ptyTabs.map(tab => (
            <div
              key={tab.id}
              className={tab.id === activeTabId
                ? 'absolute inset-0 flex flex-col z-10'
                : 'hidden'
              }
            >
              {tab.type === 'ssh' ? (
                <>
                  <div className="flex-1 min-h-0">
                    <InteractiveTerminal
                      sessionId={tab.ptySessionId!}
                      label={getTerminalHeaderLabel(tab, config)}
                      isActive={tab.id === activeTabId}
                      onExit={() => handlePtyExit(tab.ptySessionId!)}
                    />
                  </div>
                  <SSHToolbar
                    sshConnectionId={tab.sshConnectionId}
                    ptySessionId={tab.ptySessionId!}
                  />
                </>
              ) : (
                <InteractiveTerminal
                  sessionId={tab.ptySessionId!}
                  label={getTerminalHeaderLabel(tab, config)}
                  isActive={tab.id === activeTabId}
                  showActivity={tab.type === 'claude' || tab.type === 'codex'}
                  hideCursor={tab.type === 'claude' || tab.type === 'codex'}
                  onExit={() => handlePtyExit(tab.ptySessionId!)}
                />
              )}
            </div>
          ))}

          {!activeTab && (
            <div className="flex-1 flex items-center justify-center text-zinc-500">
              <div className="text-center">
                <p className="text-2xl font-semibold mb-2">No server selected</p>
                <p className="text-sm">Add a project or select a command to get started</p>
              </div>
            </div>
          )}
        </div>
        <StatusBar />
      </div>
    </div>
  );
}
