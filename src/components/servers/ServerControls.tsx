import { useState, useEffect, useRef } from 'react';
import { Play, Square, RotateCcw, ExternalLink, Zap, Check, X } from 'lucide-react';
import { open } from '@tauri-apps/plugin-shell';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../../stores/appStore';
import { useProcessStore } from '../../stores/processStore';
import { useProcess } from '../../hooks/useProcess';
import { findFolderForCommand } from '../../utils/projectHelpers';

export function ServerControls({ commandId }: { commandId: string }) {
  const config = useAppStore(s => s.config);
  const proc = useProcessStore(s => s.processes[commandId]);
  const { startProcess, stopProcess, restartProcess } = useProcess();
  const [killing, setKilling] = useState(false);
  const [killResult, setKillResult] = useState<'killed' | 'none' | 'error' | null>(null);
  const killTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => () => { if (killTimerRef.current) clearTimeout(killTimerRef.current); }, []);

  const tab = useAppStore(s => s.openTabs.find(t => t.type === 'server' && t.commandId === commandId));
  const project = config?.projects.find(p => p.id === tab?.projectId);
  const folder = project ? findFolderForCommand(project, commandId) : undefined;
  const command = folder?.commands.find(c => c.id === commandId);

  if (!project || !folder || !command) return null;

  const status = proc?.status || 'stopped';
  const port = command.port;

  const handleKillPort = async () => {
    if (!port) return;
    setKilling(true);
    setKillResult(null);
    try {
      await invoke('kill_port', { port });
      setKillResult('killed');
    } catch {
      // "No process found" means port was already free
      setKillResult('none');
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
          disabled={killing}
          className={`p-1.5 rounded hover:bg-zinc-700 disabled:opacity-50 transition-colors duration-150 ${
            killResult === 'killed' ? 'text-emerald-400 bg-emerald-400/10' :
            killResult === 'none' ? 'text-zinc-500 bg-zinc-700/50' :
            'text-orange-400 hover:text-orange-300'
          }`}
          title={
            killResult === 'killed' ? `Port ${port} freed` :
            killResult === 'none' ? `Port ${port} was not in use` :
            `Kill port ${port}`
          }
        >
          {killResult === 'killed' ? <Check size={16} /> :
           killResult === 'none' ? <X size={16} /> :
           <Zap size={16} />}
        </button>
      )}
      {port && status === 'running' && (
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
