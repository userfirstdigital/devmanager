import { useEffect, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { useProcessStore } from '../../stores/processStore';
import { Cpu, HardDrive } from 'lucide-react';
import type { ProcessTreeInfo } from '../../types/config';

function formatMemory(mb: number): string {
  if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GB`;
  return `${Math.round(mb)} MB`;
}

function memoryColor(mb: number): string {
  if (mb > 500) return 'text-red-400';
  if (mb > 100) return 'text-amber-400';
  return 'text-emerald-400';
}

export function ResourceMonitor({ commandId }: { commandId: string }) {
  const resources = useProcessStore(s => s.resources[commandId]);
  const updateResources = useProcessStore(s => s.updateResources);
  const proc = useProcessStore(s => s.processes[commandId]);

  useEffect(() => {
    const unlisten = listen<ProcessTreeInfo>('resource-update', (event) => {
      if (event.payload.command_id === commandId) {
        updateResources(commandId, event.payload);
      }
    });
    return () => { unlisten.then(fn => fn()); };
  }, [commandId]);

  if (!proc || proc.status !== 'running' || !resources) {
    // Show uptime if running
    if (proc?.status === 'running' && proc.startedAt) {
      const elapsed = Math.floor((Date.now() - proc.startedAt) / 1000);
      const hours = Math.floor(elapsed / 3600);
      const mins = Math.floor((elapsed % 3600) / 60);
      const uptimeStr = hours > 0 ? `${hours}h ${mins}m` : mins > 0 ? `${mins}m` : '< 1m';
      return (
        <div className="flex items-center gap-2 text-[11px] text-zinc-500">
          <span>Uptime: {uptimeStr}</span>
        </div>
      );
    }
    return <div />;
  }

  return (
    <div className="flex items-center gap-3 text-[11px]">
      <div className="flex items-center gap-1">
        <HardDrive size={12} className="text-zinc-500" />
        <span className={memoryColor(resources.total_memory_mb)}>
          {formatMemory(resources.total_memory_mb)}
        </span>
      </div>
      <div className="flex items-center gap-1">
        <Cpu size={12} className="text-zinc-500" />
        <span className="text-zinc-300">{resources.total_cpu_percent.toFixed(1)}%</span>
      </div>
      <span className="text-zinc-600">{resources.processes.length} processes</span>
      {proc.startedAt && (
        <UptimeDisplay startedAt={proc.startedAt} />
      )}
    </div>
  );
}

function UptimeDisplay({ startedAt }: { startedAt: number }) {
  const [, forceUpdate] = useState(0);
  useEffect(() => {
    const interval = setInterval(() => forceUpdate(n => n + 1), 30000);
    return () => clearInterval(interval);
  }, []);

  const elapsed = Math.floor((Date.now() - startedAt) / 1000);
  const days = Math.floor(elapsed / 86400);
  const hours = Math.floor((elapsed % 86400) / 3600);
  const mins = Math.floor((elapsed % 3600) / 60);
  const uptimeStr = days > 0 ? `${days}d ${hours}h` : hours > 0 ? `${hours}h ${mins}m` : mins > 0 ? `${mins}m` : '< 1m';

  return <span className="text-zinc-500">Up: {uptimeStr}</span>;
}
