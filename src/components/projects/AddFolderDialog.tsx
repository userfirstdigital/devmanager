import { useState } from 'react';
import { X, FolderOpen, Loader2, Check } from 'lucide-react';
import { open } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../../stores/appStore';
import type { ScanResult, ProjectFolder, RunCommand } from '../../types/config';

export function AddFolderDialog({ projectId, onClose }: { projectId: string; onClose: () => void }) {
  const [folderPath, setFolderPath] = useState('');
  const [scanning, setScanning] = useState(false);
  const [scanResult, setScanResult] = useState<ScanResult | null>(null);
  const [selectedScripts, setSelectedScripts] = useState<Set<string>>(new Set());
  const [selectedPortVar, setSelectedPortVar] = useState<string | null>(null);

  const config = useAppStore(s => s.config);
  const updateProject = useAppStore(s => s.updateProject);

  const handlePickFolder = async () => {
    const selected = await open({ directory: true, multiple: false });
    if (selected && typeof selected === 'string') {
      setFolderPath(selected);

      setScanning(true);
      try {
        const result = await invoke<ScanResult>('scan_project', { folderPath: selected });
        setScanResult(result);
        const autoSelect = new Set<string>();
        result.scripts.forEach(s => {
          if (['dev', 'start', 'serve'].includes(s.name)) autoSelect.add(s.name);
        });
        setSelectedScripts(autoSelect);
        if (result.ports.length === 1) {
          setSelectedPortVar(result.ports[0].variable);
        } else {
          setSelectedPortVar(null);
        }
      } catch (err) {
        console.error('Scan failed:', err);
      }
      setScanning(false);
    }
  };

  const handleAdd = async () => {
    if (!folderPath) return;
    const project = config?.projects.find(p => p.id === projectId);
    if (!project) return;

    const selectedPort = scanResult?.ports.find(p => p.variable === selectedPortVar);

    const commands: RunCommand[] = [];
    selectedScripts.forEach(scriptName => {
      const script = scanResult?.scripts.find(s => s.name === scriptName);
      if (script) {
        commands.push({
          id: crypto.randomUUID(),
          label: scriptName,
          command: 'npm',
          args: ['run', scriptName],
          port: selectedPort?.port || undefined,
          autoRestart: false,
          clearLogsOnRestart: true,
        });
      }
    });

    const folderName = folderPath.split(/[/\\]/).filter(Boolean).pop() || 'folder';

    const folder: ProjectFolder = {
      id: crypto.randomUUID(),
      name: folderName,
      folderPath,
      commands,
      envFilePath: scanResult?.has_env_file ? '.env' : undefined,
      portVariable: selectedPortVar || undefined,
    };

    await updateProject({
      ...project,
      folders: [...project.folders, folder],
      updatedAt: new Date().toISOString(),
    });
    onClose();
  };

  return (
    <div className="fixed inset-0 bg-black/60 flex items-center justify-center z-50" onClick={onClose}>
      <div className="bg-zinc-800 rounded-lg border border-zinc-700 shadow-xl w-[500px] max-h-[80vh] overflow-hidden" onClick={e => e.stopPropagation()}>
        <div className="flex items-center justify-between p-4 border-b border-zinc-700">
          <h2 className="text-sm font-semibold text-zinc-100">Add Folder</h2>
          <button onClick={onClose} className="p-1 hover:bg-zinc-700 rounded text-zinc-400">
            <X size={16} />
          </button>
        </div>

        <div className="p-4 space-y-4 overflow-y-auto max-h-[60vh]">
          {/* Folder picker */}
          <div>
            <label className="text-xs text-zinc-400 mb-1 block">Folder</label>
            <button
              onClick={handlePickFolder}
              className="w-full flex items-center gap-2 px-3 py-2 bg-zinc-900 border border-zinc-700 rounded text-xs text-zinc-300 hover:border-zinc-500"
            >
              <FolderOpen size={14} />
              {folderPath || 'Select folder...'}
            </button>
          </div>

          {scanning && (
            <div className="flex items-center gap-2 text-xs text-zinc-400">
              <Loader2 size={14} className="animate-spin" />
              Scanning folder...
            </div>
          )}

          {scanResult && (
            <div>
              <label className="text-xs text-zinc-400 mb-1 block">npm Scripts ({scanResult.scripts.length} found)</label>
              <div className="space-y-1 max-h-48 overflow-y-auto">
                {scanResult.scripts.map(script => (
                  <div
                    key={script.name}
                    className="flex items-center gap-2 px-2 py-1.5 rounded hover:bg-zinc-700/50 cursor-pointer"
                    onClick={() => setSelectedScripts(prev => {
                      const next = new Set(prev);
                      if (next.has(script.name)) next.delete(script.name);
                      else next.add(script.name);
                      return next;
                    })}
                  >
                    <div className={`w-4 h-4 rounded border flex items-center justify-center ${
                      selectedScripts.has(script.name) ? 'bg-indigo-600 border-indigo-600' : 'border-zinc-600'
                    }`}>
                      {selectedScripts.has(script.name) && <Check size={12} className="text-white" />}
                    </div>
                    <span className="text-xs text-zinc-200 font-mono">{script.name}</span>
                    <span className="text-[10px] text-zinc-500 truncate ml-auto">{script.command}</span>
                  </div>
                ))}
              </div>
            </div>
          )}

          {scanResult && scanResult.ports.length > 0 && (
            <div>
              <label className="text-xs text-zinc-400 mb-1 block">
                Port Variable {scanResult.ports.length > 1 ? '(select one)' : ''}
              </label>
              <div className="space-y-1">
                {scanResult.ports.map(p => (
                  <div
                    key={p.variable}
                    className={`flex items-center gap-2 px-2 py-1.5 rounded cursor-pointer ${
                      selectedPortVar === p.variable ? 'bg-indigo-600/20 border border-indigo-500/50' : 'hover:bg-zinc-700/50 border border-transparent'
                    }`}
                    onClick={() => setSelectedPortVar(p.variable)}
                  >
                    <div className={`w-3.5 h-3.5 rounded-full border-2 flex items-center justify-center ${
                      selectedPortVar === p.variable ? 'border-indigo-500' : 'border-zinc-600'
                    }`}>
                      {selectedPortVar === p.variable && <div className="w-1.5 h-1.5 rounded-full bg-indigo-500" />}
                    </div>
                    <span className="text-xs font-mono text-indigo-400">{p.variable}</span>
                    <span className="text-xs text-zinc-300">= {p.port}</span>
                    <span className="text-[10px] text-zinc-500 ml-auto">({p.source})</span>
                  </div>
                ))}
              </div>
            </div>
          )}

          {scanResult && !scanResult.has_package_json && (
            <div className="text-xs text-amber-400 bg-amber-400/10 px-3 py-2 rounded">
              No package.json found in this folder
            </div>
          )}
        </div>

        <div className="flex justify-end gap-2 p-4 border-t border-zinc-700">
          <button
            onClick={onClose}
            className="px-4 py-1.5 text-xs text-zinc-400 hover:text-zinc-100"
          >
            Cancel
          </button>
          <button
            onClick={handleAdd}
            disabled={!folderPath}
            className="px-4 py-1.5 bg-indigo-600 hover:bg-indigo-500 disabled:opacity-50 disabled:cursor-not-allowed text-white text-xs font-medium rounded"
          >
            Add Folder
          </button>
        </div>
      </div>
    </div>
  );
}
