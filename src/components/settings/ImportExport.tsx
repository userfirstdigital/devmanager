import { useState } from 'react';
import { X, Download, Upload, Loader2 } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import { save, open as openDialog } from '@tauri-apps/plugin-dialog';
import { writeTextFile, readTextFile } from '@tauri-apps/plugin-fs';
import { useAppStore } from '../../stores/appStore';
import type { AppConfig, ProjectFolder } from '../../types/config';

export function ImportExport({ onClose }: { onClose: () => void }) {
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const loadConfig = useAppStore(s => s.loadConfig);
  const config = useAppStore(s => s.config);

  const handleExport = async () => {
    setLoading(true);
    setError(null);
    setStatus(null);
    try {
      const currentConfig = await invoke<AppConfig>('get_config');
      const path = await save({
        defaultPath: `devmanager-config-${new Date().toISOString().slice(0, 10)}.json`,
        filters: [{ name: 'JSON', extensions: ['json'] }],
      });
      if (path) {
        await writeTextFile(path, JSON.stringify(currentConfig, null, 2));
        setStatus('Configuration exported successfully.');
      }
    } catch (err) {
      setError(`Export failed: ${err}`);
    }
    setLoading(false);
  };

  const handleImport = async (mode: 'merge' | 'replace') => {
    setLoading(true);
    setError(null);
    setStatus(null);
    try {
      const path = await openDialog({
        multiple: false,
        filters: [{ name: 'JSON', extensions: ['json'] }],
      });
      if (path && typeof path === 'string') {
        const text = await readTextFile(path);
        const imported = JSON.parse(text) as AppConfig;

        if (!imported.projects || !imported.settings) {
          setError('Invalid configuration file.');
          setLoading(false);
          return;
        }

        // Migrate v1 imports: convert old folderPath/commands/envFilePath to folders[]
        const migrateProject = (p: any) => {
          if (p.folders) return p; // already v2
          const folderPath = p.folderPath || '';
          const folderName = folderPath.split(/[/\\]/).filter(Boolean).pop() || 'folder';
          const folder: ProjectFolder = {
            id: crypto.randomUUID(),
            name: folderName,
            folderPath,
            commands: p.commands || [],
            envFilePath: p.envFilePath,
          };
          const { folderPath: _, commands: __, envFilePath: ___, ...rest } = p;
          return { ...rest, folders: [folder] };
        };
        imported.projects = imported.projects.map(migrateProject);
        imported.version = 2;

        if (mode === 'replace') {
          await invoke('save_full_config', { config: imported });
          setStatus('Configuration replaced successfully.');
        } else {
          // Merge: add projects that don't already exist by name
          const existingNames = new Set(config?.projects.map(p => p.name) ?? []);
          const newProjects = imported.projects.filter(p => !existingNames.has(p.name));
          const mergedConfig: AppConfig = {
            ...config!,
            projects: [...(config?.projects ?? []), ...newProjects],
          };
          await invoke('save_full_config', { config: mergedConfig });
          setStatus(`Merged ${newProjects.length} new project(s).`);
        }

        await loadConfig();
      }
    } catch (err) {
      setError(`Import failed: ${err}`);
    }
    setLoading(false);
  };

  return (
    <div className="fixed inset-0 bg-black/60 flex items-center justify-center z-50" onClick={onClose}>
      <div
        className="bg-zinc-800 rounded-lg border border-zinc-700 shadow-xl w-[400px] flex flex-col"
        onClick={e => e.stopPropagation()}
      >
        <div className="flex items-center justify-between p-4 border-b border-zinc-700">
          <h2 className="text-sm font-semibold text-zinc-100">Import / Export</h2>
          <button onClick={onClose} className="p-1 hover:bg-zinc-700 rounded text-zinc-400">
            <X size={16} />
          </button>
        </div>

        <div className="p-4 space-y-4">
          <div>
            <h3 className="text-xs text-zinc-300 font-medium mb-2">Export</h3>
            <button
              onClick={handleExport}
              disabled={loading}
              className="flex items-center gap-2 px-4 py-2 bg-zinc-700 hover:bg-zinc-600 disabled:opacity-50 text-zinc-200 text-xs rounded w-full"
            >
              {loading ? <Loader2 size={14} className="animate-spin" /> : <Download size={14} />}
              Export Configuration to File
            </button>
          </div>

          <div className="border-t border-zinc-700 pt-4">
            <h3 className="text-xs text-zinc-300 font-medium mb-2">Import</h3>
            <div className="space-y-2">
              <button
                onClick={() => handleImport('merge')}
                disabled={loading}
                className="flex items-center gap-2 px-4 py-2 bg-zinc-700 hover:bg-zinc-600 disabled:opacity-50 text-zinc-200 text-xs rounded w-full"
              >
                {loading ? <Loader2 size={14} className="animate-spin" /> : <Upload size={14} />}
                Import &amp; Merge (add new projects only)
              </button>
              <button
                onClick={() => handleImport('replace')}
                disabled={loading}
                className="flex items-center gap-2 px-4 py-2 bg-red-900/50 hover:bg-red-800/50 disabled:opacity-50 text-red-300 text-xs rounded w-full"
              >
                {loading ? <Loader2 size={14} className="animate-spin" /> : <Upload size={14} />}
                Import &amp; Replace (overwrite entire config)
              </button>
            </div>
          </div>

          {status && (
            <div className="text-xs text-emerald-400 bg-emerald-400/10 px-3 py-2 rounded">{status}</div>
          )}
          {error && (
            <div className="text-xs text-red-400 bg-red-400/10 px-3 py-2 rounded">{error}</div>
          )}
        </div>

        <div className="flex justify-end p-4 border-t border-zinc-700">
          <button
            onClick={onClose}
            className="px-4 py-1.5 text-xs text-zinc-400 hover:text-zinc-100"
          >
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
