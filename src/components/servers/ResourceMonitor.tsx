import { useEffect, useRef, useState } from 'react';
import { useProcessStore } from '../../stores/processStore';
import { Cpu, HardDrive } from 'lucide-react';
import type { ProcessTreeInfo } from '../../types/config';
import { listenWithAutoCleanup } from '../../utils/tauriListeners';
import { useWindowMotionActive } from '../../hooks/useWindowMotion';

const METRIC_ROW_CLASS = 'flex items-center gap-3 text-[11px] font-mono tabular-nums shrink-0';
const METRIC_CELL_CLASS = 'flex items-center justify-end gap-1 shrink-0';
const MEMORY_WIDTH_CLASS = 'w-[88px]';
const CPU_WIDTH_CLASS = 'w-[72px]';
const PROCESS_WIDTH_CLASS = 'w-[84px]';
const UPTIME_WIDTH_CLASS = 'w-[70px] text-right';

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
  const windowMoving = useWindowMotionActive();
  const deferredUpdateRef = useRef<ProcessTreeInfo | null>(null);
  const movingRef = useRef(windowMoving);

  useEffect(() => {
    movingRef.current = windowMoving;
    if (!windowMoving && deferredUpdateRef.current) {
      updateResources(commandId, deferredUpdateRef.current);
      deferredUpdateRef.current = null;
    }
  }, [commandId, updateResources, windowMoving]);

  useEffect(() => {
    const unlisten = listenWithAutoCleanup<ProcessTreeInfo>('resource-update', (event) => {
      if (event.payload.command_id === commandId) {
        if (movingRef.current) {
          deferredUpdateRef.current = event.payload;
          return;
        }
        updateResources(commandId, event.payload);
      }
    });
    return () => { unlisten.then(fn => fn()); };
  }, [commandId, updateResources]);

  if (!proc || proc.status !== 'running' || !resources) {
    // Show uptime if running
    if (proc?.status === 'running' && proc.startedAt) {
      return (
        <div className={METRIC_ROW_CLASS}>
          <div className={`${METRIC_CELL_CLASS} ${MEMORY_WIDTH_CLASS}`} />
          <div className={`${METRIC_CELL_CLASS} ${CPU_WIDTH_CLASS}`} />
          <div className={`${PROCESS_WIDTH_CLASS} text-right text-zinc-600`} />
          <UptimeDisplay startedAt={proc.startedAt} className={UPTIME_WIDTH_CLASS} />
        </div>
      );
    }
    return <div />;
  }

  return (
    <div className={METRIC_ROW_CLASS}>
      <div className={`${METRIC_CELL_CLASS} ${MEMORY_WIDTH_CLASS}`}>
        <HardDrive size={12} className="text-zinc-500" />
        <span className={memoryColor(resources.total_memory_mb)}>
          {formatMemory(resources.total_memory_mb)}
        </span>
      </div>
      <div className={`${METRIC_CELL_CLASS} ${CPU_WIDTH_CLASS}`}>
        <Cpu size={12} className="text-zinc-500" />
        <span className="text-zinc-300">{resources.total_cpu_percent.toFixed(1)}%</span>
      </div>
      <span className={`${PROCESS_WIDTH_CLASS} text-right text-zinc-600`}>
        {resources.processes.length} proc
      </span>
      {proc.startedAt && (
        <UptimeDisplay startedAt={proc.startedAt} className={UPTIME_WIDTH_CLASS} />
      )}
    </div>
  );
}

function UptimeDisplay({ startedAt, className }: { startedAt: number; className?: string }) {
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

  return <span className={`text-zinc-500 ${className || ''}`}>Up:{uptimeStr}</span>;
}
