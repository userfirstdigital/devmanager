import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { AppLayout } from './components/layout/AppLayout';
import { useSessionRestore } from './hooks/useSessionRestore';
import { useAppStore } from './stores/appStore';
import { useProcessStore } from './stores/processStore';
import { useProcess } from './hooks/useProcess';
import { isMacPlatform } from './utils/runtimePlatform';
import { APP_WINDOW_TITLE, getWindowTitle } from './utils/tabTitles';

export default function App() {
  useSessionRestore();
  const loading = useAppStore(s => s.loading);
  const config = useAppStore(s => s.config);
  const runtimeInfo = useAppStore(s => s.runtimeInfo);
  const activeTabId = useAppStore(s => s.activeTabId);
  const openTabs = useAppStore(s => s.openTabs);
  const terminalTitles = useProcessStore(s => s.terminalTitles);
  const [showCloseConfirm, setShowCloseConfirm] = useState(false);
  const { stopAll } = useProcess();

  const activeTab = openTabs.find(tab => tab.id === activeTabId);
  const windowTitle = getWindowTitle(activeTab, config, terminalTitles);

  useEffect(() => {
    document.title = windowTitle || APP_WINDOW_TITLE;
    getCurrentWindow().setTitle(windowTitle || APP_WINDOW_TITLE).catch(err => {
      console.warn('Failed to sync window title:', err);
    });
  }, [windowTitle]);

  useEffect(() => {
    const unlisten = getCurrentWindow().onCloseRequested(async (event) => {
      const latestConfig = useAppStore.getState().config;
      const latestRuntimeInfo = useAppStore.getState().runtimeInfo;
      const isMac = isMacPlatform(latestRuntimeInfo);
      const confirmOnClose = latestConfig?.settings.confirmOnClose ?? true;

      // If minimize to tray is on, let Rust hide the window with no confirmation.
      if (latestConfig?.settings.minimizeToTray) return;

      const processes = useProcessStore.getState().processes;
      const hasRunning = Object.values(processes).some(process => process.status === 'running');

      if (hasRunning && confirmOnClose) {
        event.preventDefault();
        setShowCloseConfirm(true);
        return;
      }

      if (isMac && confirmOnClose) {
        event.preventDefault();
        await invoke('quit_app');
      }
    });

    return () => {
      unlisten.then(fn => fn());
    };
  }, []);

  const handleConfirmClose = async () => {
    await stopAll();
    setShowCloseConfirm(false);
    if (isMacPlatform(runtimeInfo)) {
      await invoke('quit_app');
      return;
    }
    await getCurrentWindow().destroy();
  };

  if (loading) {
    return (
      <div className="flex h-screen bg-zinc-900 text-zinc-100 items-center justify-center">
        <div className="text-center">
          <div className="text-lg font-semibold">{APP_WINDOW_TITLE}</div>
          <div className="text-sm text-zinc-500 mt-1">Loading...</div>
        </div>
      </div>
    );
  }

  return (
    <>
      <AppLayout />
      {showCloseConfirm && (
        <CloseConfirmDialog
          onConfirm={handleConfirmClose}
          onCancel={() => setShowCloseConfirm(false)}
        />
      )}
    </>
  );
}

function CloseConfirmDialog({ onConfirm, onCancel }: { onConfirm: () => void; onCancel: () => void }) {
  const runningCount = useProcessStore(s => s.getRunningCount());

  return (
    <div className="fixed inset-0 bg-black/60 flex items-center justify-center z-50" onClick={onCancel}>
      <div
        className="bg-zinc-800 rounded-lg border border-zinc-700 shadow-xl w-[380px] p-6"
        onClick={event => event.stopPropagation()}
      >
        <h2 className="text-sm font-semibold text-zinc-100 mb-2">Quit DevManager?</h2>
        <p className="text-xs text-zinc-400 mb-4">
          {runningCount} server{runningCount !== 1 ? 's are' : ' is'} still running. Stop all and quit?
        </p>
        <div className="flex justify-end gap-2">
          <button
            onClick={onCancel}
            className="px-4 py-1.5 text-xs text-zinc-400 hover:text-zinc-100"
          >
            Cancel
          </button>
          <button
            onClick={onConfirm}
            className="px-4 py-1.5 bg-red-600 hover:bg-red-500 text-white text-xs font-medium rounded"
          >
            Stop All &amp; Quit
          </button>
        </div>
      </div>
    </div>
  );
}
