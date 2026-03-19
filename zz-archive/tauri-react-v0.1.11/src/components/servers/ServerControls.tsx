import { useState, useEffect, useRef, useCallback } from 'react';
import { Play, Square, RotateCcw, ExternalLink, Zap, Check, X } from 'lucide-react';
import { open } from '@tauri-apps/plugin-shell';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../../stores/appStore';
import { useProcessStore } from '../../stores/processStore';
import { useProcess } from '../../hooks/useProcess';
import type { PortStatus } from '../../types/config';
import { findFolderForCommand } from '../../utils/projectHelpers';
import { writeToSessionTerminal } from '../../utils/terminalBuffers';
import { useWindowMotionActive } from '../../hooks/useWindowMotion';

export function ServerControls({ commandId }: { commandId: string }) {
  const config = useAppStore(s => s.config);
  const proc = useProcessStore(s => s.processes[commandId]);
  const resources = useProcessStore(s => s.resources[commandId]);
  const { startProcess, stopProcess, stopProcessAndWait, restartProcess } = useProcess();
  const windowMoving = useWindowMotionActive();
  const [killing, setKilling] = useState(false);
  const [killResult, setKillResult] = useState<'killed' | 'none' | 'error' | null>(null);
  const [portInUse, setPortInUse] = useState<{
    pid?: number;
    processName?: string;
  } | null>(null);
  const killTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => () => { if (killTimerRef.current) clearTimeout(killTimerRef.current); }, []);

  const tab = useAppStore(s => s.openTabs.find(t => t.type === 'server' && t.commandId === commandId));
  const project = config?.projects.find(p => p.id === tab?.projectId);
  const folder = project ? findFolderForCommand(project, commandId) : undefined;
  const command = folder?.commands.find(c => c.id === commandId);

  if (!project || !folder || !command) return null;

  const status = proc?.status || 'stopped';
  const port = command.port;
  const managedPids = new Set<number>();
  if (proc?.pid != null) {
    managedPids.add(proc.pid);
  }
  for (const child of resources?.processes ?? []) {
    managedPids.add(child.pid);
  }

  const isManagedProcessOnPort = Boolean(
    portInUse?.pid != null &&
    managedPids.has(portInUse.pid)
  );
  const hasPortConflict = Boolean(
    port &&
    portInUse &&
    !isManagedProcessOnPort
  );
  const isManagedProcessActive =
    status === 'running' ||
    status === 'starting' ||
    status === 'stopping';
  const resolveActionLabel = isManagedProcessActive ? 'restart' : 'start';

  const refreshPortStatus = useCallback(async () => {
    if (!port) {
      setPortInUse(null);
      return;
    }

    try {
      const result = await invoke<PortStatus>('check_port_in_use', { port });
      if (result.in_use) {
        setPortInUse({
          pid: result.pid,
          processName: result.process_name,
        });
      } else {
        setPortInUse(null);
      }
    } catch {
      setPortInUse(null);
    }
  }, [port]);

  useEffect(() => {
    if (!port) {
      setPortInUse(null);
      return;
    }

    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;

    const schedule = (delayMs: number) => {
      if (cancelled) return;
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        void runCheck();
      }, delayMs);
    };

    const shouldContinueBurstChecks = () => {
      if (killing || status === 'starting' || status === 'stopping') {
        return true;
      }

      if (status === 'running' && proc?.startedAt != null) {
        return Date.now() < proc.startedAt + 15_000;
      }

      return false;
    };

    const runCheck = async () => {
      if (cancelled) return;

      if (document.visibilityState !== 'visible' || windowMoving) {
        if (shouldContinueBurstChecks()) {
          schedule(500);
        }
        return;
      }

      await refreshPortStatus();

      if (shouldContinueBurstChecks()) {
        schedule(2000);
      }
    };

    void runCheck();

    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [port, proc?.startedAt, refreshPortStatus, status, killing, windowMoving]);

  useEffect(() => {
    if (!port) return;

    let refreshTimer: ReturnType<typeof setTimeout> | null = null;

    const scheduleVisibilityRefresh = () => {
      if (document.visibilityState !== 'visible') {
        return;
      }

      if (refreshTimer) {
        clearTimeout(refreshTimer);
      }

      // Avoid expensive port scans in the same activation gesture that begins
      // a native window drag on Windows.
      refreshTimer = setTimeout(() => {
        refreshTimer = null;
        if (!windowMoving) {
          void refreshPortStatus();
        }
      }, 400);
    };

    document.addEventListener('visibilitychange', scheduleVisibilityRefresh);

    return () => {
      if (refreshTimer) {
        clearTimeout(refreshTimer);
      }
      document.removeEventListener('visibilitychange', scheduleVisibilityRefresh);
    };
  }, [port, refreshPortStatus, windowMoving]);

  const handleKillPort = async () => {
    if (!port) return;
    setKilling(true);
    setKillResult(null);
    try {
      writeToSessionTerminal(
        commandId,
        `\r\n\x1b[33m--- Resolving port ${port} conflict... ---\x1b[0m\r\n`,
      );

      if (isManagedProcessActive) {
        const stopped = await stopProcessAndWait(commandId);
        if (!stopped) {
          throw new Error(`Managed process ${commandId} did not stop cleanly`);
        }
      }

      try {
        await invoke('kill_port', { port });
        setKillResult('killed');
      } catch (error) {
        const message = String(error);
        if (message.includes('No process found')) {
          setKillResult('none');
        } else {
          throw error;
        }
      }

      await refreshPortStatus();
      writeToSessionTerminal(
        commandId,
        `\x1b[33m--- Starting after freeing port ${port}... ---\x1b[0m\r\n`,
      );
      await startProcess(folder, command, project.id);
    } catch (error) {
      await refreshPortStatus();
      setKillResult('error');
      writeToSessionTerminal(
        commandId,
        `\x1b[31mFailed to resolve port ${port} conflict: ${String(error)}\x1b[0m\r\n`,
      );
    }
    setKilling(false);
    if (killTimerRef.current) clearTimeout(killTimerRef.current);
    killTimerRef.current = setTimeout(() => setKillResult(null), 2000);
  };

  return (
    <div className="flex items-center gap-1">
      {status !== 'running' ? (
        <button
          onClick={() => startProcess(folder, command, project.id)}
          disabled={status === 'starting'}
          className="p-1.5 rounded hover:bg-zinc-700 text-emerald-400 hover:text-emerald-300 disabled:opacity-50"
          title="Start"
        >
          <Play size={16} />
        </button>
      ) : (
        <>
          <button
            onClick={() => stopProcess(commandId)}
            className="p-1.5 rounded hover:bg-zinc-700 text-red-400 hover:text-red-300"
            title="Stop"
          >
            <Square size={16} />
          </button>
          <button
            onClick={() => restartProcess(folder, command, project.id)}
            className="p-1.5 rounded hover:bg-zinc-700 text-amber-400 hover:text-amber-300"
            title="Restart"
          >
            <RotateCcw size={16} />
          </button>
        </>
      )}
      {port && (
        <button
          onClick={handleKillPort}
          disabled={killing || !hasPortConflict}
          className={`p-1.5 rounded hover:bg-zinc-700 disabled:opacity-50 transition-colors duration-150 ${
            killResult === 'killed' ? 'text-emerald-400 bg-emerald-400/10' :
            killResult === 'none' ? 'text-zinc-500 bg-zinc-700/50' :
            killResult === 'error' ? 'text-red-400 bg-red-400/10' :
            hasPortConflict ? 'text-orange-400 hover:text-orange-300' : 'hidden'
          }`}
          title={
            killResult === 'killed' ? `Port ${port} freed` :
            killResult === 'none' ? `Port ${port} was not in use` :
            killResult === 'error' ? `Failed to free port ${port}` :
            hasPortConflict
              ? `Kill ${portInUse?.processName || 'process'}${portInUse?.pid ? ` (PID ${portInUse.pid})` : ''} on port ${port} and ${resolveActionLabel}`
              : `Kill port ${port}`
          }
        >
          {killResult === 'killed' ? <Check size={16} /> :
           killResult === 'none' ? <X size={16} /> :
           <Zap size={16} />}
        </button>
      )}
      {port && status === 'running' && !hasPortConflict && (
        <button
          onClick={() => open(`http://localhost:${port}`)}
          className="p-1.5 rounded hover:bg-zinc-700 text-indigo-400 hover:text-indigo-300"
          title={`Open http://localhost:${port}`}
        >
          <ExternalLink size={16} />
        </button>
      )}
    </div>
  );
}
