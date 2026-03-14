import { useProcessStore } from '../../stores/processStore';
import { Activity, Server, ArrowUpCircle, Loader2, Check } from 'lucide-react';
import { useState, useEffect } from 'react';
import type { Update } from '@tauri-apps/plugin-updater';
import { relaunch } from '@tauri-apps/plugin-process';

interface UpdateDetail {
  version: string;
  body: string;
  update: Update;
}

export function StatusBar() {
  const runningCount = useProcessStore(s => s.getRunningCount());
  const totalMemory = useProcessStore(s => s.getTotalMemory());
  const [time, setTime] = useState(new Date());
  const [updateDetail, setUpdateDetail] = useState<UpdateDetail | null>(null);
  const [updating, setUpdating] = useState(false);
  const [downloaded, setDownloaded] = useState(false);
  const [progress, setProgress] = useState<number | null>(null);

  useEffect(() => {
    const interval = setInterval(() => setTime(new Date()), 60000);
    return () => clearInterval(interval);
  }, []);

  useEffect(() => {
    const handler = (e: Event) => {
      setUpdateDetail((e as CustomEvent).detail);
    };
    window.addEventListener('devmanager-update-available', handler);
    return () => window.removeEventListener('devmanager-update-available', handler);
  }, []);

  const handleUpdate = async () => {
    if (!updateDetail) return;
    setUpdating(true);
    setProgress(0);
    try {
      let totalLen = 0;
      let downloadedLen = 0;
      await updateDetail.update.downloadAndInstall((event) => {
        switch (event.event) {
          case 'Started':
            totalLen = event.data.contentLength ?? 0;
            break;
          case 'Progress':
            downloadedLen += event.data.chunkLength;
            if (totalLen > 0) {
              setProgress(Math.round((downloadedLen / totalLen) * 100));
            }
            break;
          case 'Finished':
            setDownloaded(true);
            break;
        }
      });
      // Relaunch after install
      await relaunch();
    } catch (err) {
      console.error('Update failed:', err);
      setUpdating(false);
      setProgress(null);
    }
  };

  const formatMemory = (mb: number): string => {
    if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GB`;
    if (mb > 0) return `${Math.round(mb)} MB`;
    return '0 MB';
  };

  return (
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
        {updateDetail && !updating && !downloaded && (
          <button
            onClick={handleUpdate}
            className="flex items-center gap-1 text-indigo-400 hover:text-indigo-300"
          >
            <ArrowUpCircle size={12} />
            <span>v{updateDetail.version} — click to update</span>
          </button>
        )}
        {updating && !downloaded && (
          <div className="flex items-center gap-1 text-indigo-400">
            <Loader2 size={12} className="animate-spin" />
            <span>Updating{progress !== null ? ` ${progress}%` : '...'}</span>
          </div>
        )}
        {downloaded && (
          <div className="flex items-center gap-1 text-emerald-400">
            <Check size={12} />
            <span>Restarting...</span>
          </div>
        )}
        <span>{time.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}</span>
      </div>
    </div>
  );
}
