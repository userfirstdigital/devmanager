import { useState } from 'react';
import { X, Loader2, Check, Plus, Trash2 } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../../stores/appStore';
import type { Project, ProjectFolder, RunCommand, ScanResult } from '../../types/config';

export function EditFolderDialog({ project, folder, onClose }: { project: Project; folder: ProjectFolder; onClose: () => void }) {
  const [name, setName] = useState(folder.name);
  const [commands, setCommands] = useState<RunCommand[]>(folder.commands);
  const [portVariable, setPortVariable] = useState<string | null>(folder.portVariable ?? null);
  const [scanning, setScanning] = useState(false);
  const [scanResult, setScanResult] = useState<ScanResult | null>(null);
  const [addingScript, setAddingScript] = useState(false);
  const [selectedNewScripts, setSelectedNewScripts] = useState<Set<string>>(new Set());
  const updateProject = useAppStore(s => s.updateProject);

  const handleRescan = async () => {
    setScanning(true);
    try {
      const result = await invoke<ScanResult>('scan_project', { folderPath: folder.folderPath });
      setScanResult(result);
      setAddingScript(true);
      setSelectedNewScripts(new Set());
    } catch (err) {
      console.error('Scan failed:', err);
    }
    setScanning(false);
  };

  const handleAddSelectedScripts = () => {
    if (!scanResult) return;
    const selectedPort = scanResult.ports.find(p => p.variable === portVariable);
    const newCommands: RunCommand[] = [];
    selectedNewScripts.forEach(scriptName => {
      const script = scanResult.scripts.find(s => s.name === scriptName);
      if (script && !commands.some(c => c.label === scriptName)) {
        newCommands.push({
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
    setCommands([...commands, ...newCommands]);
    setAddingScript(false);
    setSelectedNewScripts(new Set());
  };

  const handleRemoveCommand = (cmdId: string) => {
    setCommands(commands.filter(c => c.id !== cmdId));
  };

  const handleCommandLabelChange = (cmdId: string, label: string) => {
    setCommands(commands.map(c => c.id === cmdId ? { ...c, label } : c));
  };

  const handleCommandPortChange = (cmdId: string, port: string) => {
    const portNum = port ? parseInt(port, 10) : undefined;
    setCommands(commands.map(c => c.id === cmdId ? { ...c, port: portNum && !isNaN(portNum) ? portNum : undefined } : c));
  };

  const handleSave = async () => {
    if (!name.trim()) return;
    const updatedFolder: ProjectFolder = {
      ...folder,
      name: name.trim(),
      commands,
      portVariable: portVariable || undefined,
    };
    const updatedProject: Project = {
      ...project,
      folders: project.folders.map(f => f.id === folder.id ? updatedFolder : f),
      updatedAt: new Date().toISOString(),
    };
    await updateProject(updatedProject);
    onClose();
  };

  // Scripts from scan that aren't already commands
  const availableNewScripts = scanResult?.scripts.filter(
    s => !commands.some(c => c.label === s.name)
  ) ?? [];

  return (
    <div className="fixed inset-0 bg-black/60 flex items-center justify-center z-50" onClick={onClose}>
      <div className="bg-zinc-800 rounded-lg border border-zinc-700 shadow-xl w-[500px] max-h-[80vh] overflow-hidden" onClick={e => e.stopPropagation()}>
        <div className="flex items-center justify-between p-4 border-b border-zinc-700">
          <h2 className="text-sm font-semibold text-zinc-100">Edit Folder</h2>
          <button onClick={onClose} className="p-1 hover:bg-zinc-700 rounded text-zinc-400">
            <X size={16} />
          </button>
        </div>

        <div className="p-4 space-y-4 overflow-y-auto max-h-[60vh]">
          {/* Folder name */}
          <div>
            <label className="text-xs text-zinc-400 mb-1 block">Folder Name</label>
            <input
              type="text"
              value={name}
              onChange={e => setName(e.target.value)}
              className="w-full px-3 py-2 bg-zinc-900 border border-zinc-700 rounded text-xs text-zinc-100 focus:outline-none focus:border-indigo-500"
              autoFocus
            />
          </div>

          {/* Folder path (read-only) */}
          <div>
            <label className="text-xs text-zinc-400 mb-1 block">Path</label>
            <div className="px-3 py-2 bg-zinc-900/50 border border-zinc-700/50 rounded text-xs text-zinc-500 font-mono truncate">
              {folder.folderPath}
            </div>
          </div>

          {/* Commands */}
          <div>
            <div className="flex items-center justify-between mb-1">
              <label className="text-xs text-zinc-400">Commands ({commands.length})</label>
              <button
                onClick={handleRescan}
                disabled={scanning}
                className="flex items-center gap-1 text-[10px] text-indigo-400 hover:text-indigo-300 disabled:opacity-50"
              >
                {scanning ? <Loader2 size={10} className="animate-spin" /> : <Plus size={10} />}
                {scanning ? 'Scanning...' : 'Scan for scripts'}
              </button>
            </div>
            <div className="space-y-1">
              {commands.map(cmd => (
                <div key={cmd.id} className="flex items-center gap-2 px-2 py-1.5 bg-zinc-900 rounded border border-zinc-700/50">
                  <input
                    type="text"
                    value={cmd.label}
                    onChange={e => handleCommandLabelChange(cmd.id, e.target.value)}
                    className="flex-1 bg-transparent text-xs text-zinc-200 focus:outline-none min-w-0"
                  />
                  <input
                    type="text"
                    value={cmd.port ?? ''}
                    onChange={e => handleCommandPortChange(cmd.id, e.target.value)}
                    placeholder="port"
                    className="w-16 bg-zinc-800 border border-zinc-700 rounded px-1.5 py-0.5 text-[10px] text-zinc-300 focus:outline-none focus:border-indigo-500 text-center"
                  />
                  <button
                    onClick={() => handleRemoveCommand(cmd.id)}
                    className="p-0.5 rounded hover:bg-zinc-700 text-zinc-500 hover:text-red-400 flex-shrink-0"
                  >
                    <Trash2 size={12} />
                  </button>
                </div>
              ))}
              {commands.length === 0 && (
                <div className="text-xs text-zinc-500 text-center py-2">No commands. Use "Scan for scripts" to add some.</div>
              )}
            </div>
          </div>

          {/* Add scripts from scan */}
          {addingScript && availableNewScripts.length > 0 && (
            <div className="border border-zinc-700 rounded p-3 space-y-2">
              <label className="text-xs text-zinc-400 block">Add scripts from scan</label>
              <div className="space-y-1 max-h-32 overflow-y-auto">
                {availableNewScripts.map(script => (
                  <div
                    key={script.name}
                    className="flex items-center gap-2 px-2 py-1 rounded hover:bg-zinc-700/50 cursor-pointer"
                    onClick={() => setSelectedNewScripts(prev => {
                      const next = new Set(prev);
                      if (next.has(script.name)) next.delete(script.name);
                      else next.add(script.name);
                      return next;
                    })}
                  >
                    <div className={`w-3.5 h-3.5 rounded border flex items-center justify-center ${
                      selectedNewScripts.has(script.name) ? 'bg-indigo-600 border-indigo-600' : 'border-zinc-600'
                    }`}>
                      {selectedNewScripts.has(script.name) && <Check size={10} className="text-white" />}
                    </div>
                    <span className="text-xs text-zinc-200 font-mono">{script.name}</span>
                    <span className="text-[10px] text-zinc-500 truncate ml-auto">{script.command}</span>
                  </div>
                ))}
              </div>
              <div className="flex justify-end gap-2">
                <button onClick={() => setAddingScript(false)} className="text-[10px] text-zinc-400 hover:text-zinc-200">
                  Cancel
                </button>
                <button
                  onClick={handleAddSelectedScripts}
                  disabled={selectedNewScripts.size === 0}
                  className="text-[10px] text-indigo-400 hover:text-indigo-300 disabled:opacity-50"
                >
                  Add {selectedNewScripts.size} script(s)
                </button>
              </div>
            </div>
          )}

          {addingScript && availableNewScripts.length === 0 && scanResult && (
            <div className="text-xs text-zinc-500 bg-zinc-900 px-3 py-2 rounded">
              No new scripts found. All scanned scripts are already added.
            </div>
          )}

          {/* Port variable selection (from scan) */}
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
                      portVariable === p.variable ? 'bg-indigo-600/20 border border-indigo-500/50' : 'hover:bg-zinc-700/50 border border-transparent'
                    }`}
                    onClick={() => setPortVariable(p.variable)}
                  >
                    <div className={`w-3.5 h-3.5 rounded-full border-2 flex items-center justify-center ${
                      portVariable === p.variable ? 'border-indigo-500' : 'border-zinc-600'
                    }`}>
                      {portVariable === p.variable && <div className="w-1.5 h-1.5 rounded-full bg-indigo-500" />}
                    </div>
                    <span className="text-xs font-mono text-indigo-400">{p.variable}</span>
                    <span className="text-xs text-zinc-300">= {p.port}</span>
                    <span className="text-[10px] text-zinc-500 ml-auto">({p.source})</span>
                  </div>
                ))}
              </div>
            </div>
          )}
        </div>

        <div className="flex justify-end gap-2 p-4 border-t border-zinc-700">
          <button onClick={onClose} className="px-4 py-1.5 text-xs text-zinc-400 hover:text-zinc-100">
            Cancel
          </button>
          <button
            onClick={handleSave}
            disabled={!name.trim()}
            className="px-4 py-1.5 bg-indigo-600 hover:bg-indigo-500 disabled:opacity-50 disabled:cursor-not-allowed text-white text-xs font-medium rounded"
          >
            Save
          </button>
        </div>
      </div>
    </div>
  );
}
