import { useEffect } from 'react';
import { Sidebar } from './Sidebar';
import { StatusBar } from './StatusBar';
import { InteractiveTerminal } from '../terminal/InteractiveTerminal';
import { SSHToolbar } from '../ssh/SSHToolbar';
import { ServerControls } from '../servers/ServerControls';
import { useAppStore } from '../../stores/appStore';
import { useProcessStore } from '../../stores/processStore';
import { getTerminalHeaderLabel } from '../../utils/tabTitles';

export function AppLayout() {
  const activeTabId = useAppStore(s => s.activeTabId);
  const openTabs = useAppStore(s => s.openTabs);
  const config = useAppStore(s => s.config);

  const activeTab = openTabs.find(t => t.id === activeTabId);
  const activePtyTab = activeTab?.ptySessionId ? activeTab : null;

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

  return (
    <div className="flex h-screen bg-zinc-900 text-zinc-100 overflow-hidden">
      <Sidebar />
      <div className="flex flex-col flex-1 min-w-0">
        <div className="flex-1 min-h-0 flex flex-col relative">
          {activePtyTab && (
            <div key={activePtyTab.id} className="absolute inset-0 flex flex-col z-10">
              {activePtyTab.type === 'ssh' ? (
                <>
                  <div className="flex-1 min-h-0">
                    <InteractiveTerminal
                      sessionId={activePtyTab.ptySessionId!}
                      label={getTerminalHeaderLabel(activePtyTab, config)}
                      isActive
                      onExit={() => handlePtyExit(activePtyTab.ptySessionId!)}
                    />
                  </div>
                  <SSHToolbar
                    sshConnectionId={activePtyTab.sshConnectionId}
                    ptySessionId={activePtyTab.ptySessionId!}
                  />
                </>
              ) : (
                <InteractiveTerminal
                  sessionId={activePtyTab.ptySessionId!}
                  label={getTerminalHeaderLabel(activePtyTab, config)}
                  isActive
                  showActivity={activePtyTab.type === 'claude' || activePtyTab.type === 'codex'}
                  hideCursor={activePtyTab.type === 'claude' || activePtyTab.type === 'codex'}
                  headerActions={activePtyTab.type === 'server'
                    ? <ServerControls commandId={activePtyTab.commandId!} />
                    : undefined}
                  onExit={() => handlePtyExit(activePtyTab.ptySessionId!)}
                />
              )}
            </div>
          )}

          {!activePtyTab && (
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
