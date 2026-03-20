import { useState, useEffect } from 'react';
import { Activity, Server, ArrowUpCircle, Loader2 } from 'lucide-react';
import { useProcess } from '../../hooks/useProcess';
import { useUpdateCheck } from '../../hooks/useUpdateCheck';
import { useProcessStore } from '../../stores/processStore';

export function StatusBar() {
  const runningCount = useProcessStore(s => s.getRunningCount());
  const totalMemory = useProcessStore(s => s.getTotalMemory());
  const [time, setTime] = useState(new Date());
  const [showRestartConfirm, setShowRestartConfirm] = useState(false);
  const [restarting, setRestarting] = useState(false);
  const { stopAllAndWait } = useProcess();
  const {
    phase,
    version,
    progress,
    error,
    restartToUpdate,
  } = useUpdateCheck();

  useEffect(() => {
    const interval = setInterval(() => setTime(new Date()), 60000);
    return () => clearInterval(interval);
  }, []);

  const performRestart = async (stopRunningServers: boolean) => {
    setRestarting(true);
    setShowRestartConfirm(false);

    try {
      if (stopRunningServers) {
        await stopAllAndWait();
      }

      await restartToUpdate();
    } catch (err) {
      console.error('Update restart failed:', err);
      setRestarting(false);
      return;
    }

    setRestarting(false);
  };

  const handleRestartClick = async () => {
    if (phase !== 'ready' || restarting) return;

    if (runningCount > 0) {
      setShowRestartConfirm(true);
      return;
    }

    await performRestart(false);
  };

  const formatMemory = (mb: number): string => {
    if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GB`;
    if (mb > 0) return `${Math.round(mb)} MB`;
    return '0 MB';
  };

  return (
    <>
      <div className="flex items-center justify-between px-3 py-1 bg-zinc-950 border-t border-zinc-800 text-[11px] text-zinc-500">
        <div className="flex items-center gap-4">
          <div className="flex items-center gap-1.5">
            <Server size={12} />
            <span>{runningCount} server{runningCount !== 1 ? 's' : ''} running</span>
          </div>
          {runningCount > 0 && (
            <div className="flex items-center gap-1.5">
              <Activity size={12} />
              <span>{formatMemory(totalMemory)}</span>
            </div>
          )}
        </div>
        <div className="flex items-center gap-4">
          {phase === 'error' && error && (
            <span className="text-rose-400" title={error}>
              Update failed, retrying later
            </span>
          )}
          {phase === 'downloading' && (
            <div className="flex items-center gap-1 text-indigo-400">
              <Loader2 size={12} className="animate-spin" />
              <span>Downloading update{progress !== null ? ` ${progress}%` : '...'}</span>
            </div>
          )}
          {phase === 'ready' && !restarting && version && (
            <>
              {error && (
                <span className="text-rose-400" title={error}>
                  Restart failed
                </span>
              )}
              <button
                onClick={() => { void handleRestartClick(); }}
                className="flex items-center gap-1 text-indigo-400 hover:text-indigo-300"
                title={`Update ${version} is ready to install`}
              >
                <ArrowUpCircle size={12} />
                <span>Restart to update v{version}</span>
              </button>
            </>
          )}
          {restarting && (
            <div className="flex items-center gap-1 text-indigo-400">
              <Loader2 size={12} className="animate-spin" />
              <span>Restarting to update...</span>
            </div>
          )}
          <span>{time.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}</span>
        </div>
      </div>
      {showRestartConfirm && version && (
        <UpdateRestartDialog
          version={version}
          runningCount={runningCount}
          onCancel={() => setShowRestartConfirm(false)}
          onConfirm={() => { void performRestart(true); }}
        />
      )}
    </>
  );
}

function UpdateRestartDialog({
  version,
  runningCount,
  onCancel,
  onConfirm,
}: {
  version: string;
  runningCount: number;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div className="fixed inset-0 bg-black/60 flex items-center justify-center z-50" onClick={onCancel}>
      <div
        className="bg-zinc-800 rounded-lg border border-zinc-700 shadow-xl w-[420px] p-6"
        onClick={e => e.stopPropagation()}
      >
        <h2 className="text-sm font-semibold text-zinc-100 mb-2">Install update v{version}?</h2>
        <p className="text-xs text-zinc-400 mb-4">
          {runningCount} server{runningCount !== 1 ? 's are' : ' is'} still running. Stop all running servers and restart DevManager to install the update?
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
            className="px-4 py-1.5 bg-indigo-600 hover:bg-indigo-500 text-white text-xs font-medium rounded"
          >
            Stop All &amp; Restart
          </button>
        </div>
      </div>
    </div>
  );
}
