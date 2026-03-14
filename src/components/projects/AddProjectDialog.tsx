import { useState } from 'react';
import { X, FolderOpen, Loader2, Check, ArrowRight, ArrowLeft } from 'lucide-react';
import { open } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../../stores/appStore';
import type { RootScanEntry, Project, ProjectFolder, RunCommand } from '../../types/config';

const PALETTE = ['#6366f1', '#ec4899', '#f59e0b', '#10b981', '#3b82f6', '#ef4444', '#8b5cf6', '#14b8a6'];

export function AddProjectDialog({ onClose }: { onClose: () => void }) {
  const [step, setStep] = useState<1 | 2 | 3>(1);
  const [projectName, setProjectName] = useState('');
  const [color, setColor] = useState(PALETTE[Math.floor(Math.random() * PALETTE.length)]);

  // Step 2: root folder
  const [rootPath, setRootPath] = useState('');
  const [scanning, setScanning] = useState(false);
  const [scanEntries, setScanEntries] = useState<RootScanEntry[]>([]);

  // Step 3: folder selection and script config
  const [selectedFolders, setSelectedFolders] = useState<Set<string>>(new Set());
  const [folderScripts, setFolderScripts] = useState<Record<string, Set<string>>>({});
  const [folderPortVars, setFolderPortVars] = useState<Record<string, string | null>>({});

  const addProject = useAppStore(s => s.addProject);

  const handlePickRoot = async () => {
    const selected = await open({ directory: true, multiple: false });
    if (selected && typeof selected === 'string') {
      setRootPath(selected);
      setScanning(true);
      try {
        const entries = await invoke<RootScanEntry[]>('scan_root', { rootPath: selected });
        setScanEntries(entries);
        // Auto-select all discovered folders
        const allPaths = new Set(entries.map(e => e.path));
        setSelectedFolders(allPaths);
        // Auto-select dev/start/serve scripts for each folder
        const scripts: Record<string, Set<string>> = {};
        for (const entry of entries) {
          const auto = new Set<string>();
          entry.scripts.forEach(s => {
            if (['dev', 'start', 'serve'].includes(s.name)) auto.add(s.name);
          });
          scripts[entry.path] = auto;
        }
        setFolderScripts(scripts);
        // Auto-select port variable if only one per folder
        const portVars: Record<string, string | null> = {};
        for (const entry of entries) {
          portVars[entry.path] = entry.ports.length === 1 ? entry.ports[0].variable : null;
        }
        setFolderPortVars(portVars);
      } catch (err) {
        console.error('Root scan failed:', err);
      }
      setScanning(false);
    }
  };

  const toggleFolder = (path: string) => {
    setSelectedFolders(prev => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  };

  const toggleScript = (folderPath: string, scriptName: string) => {
    setFolderScripts(prev => {
      const current = prev[folderPath] || new Set();
      const next = new Set(current);
      if (next.has(scriptName)) next.delete(scriptName);
      else next.add(scriptName);
      return { ...prev, [folderPath]: next };
    });
  };

  const handleAdd = async () => {
    if (!projectName || !rootPath) return;

    const folders: ProjectFolder[] = [];

    for (const entry of scanEntries) {
      const isSelected = selectedFolders.has(entry.path);
      const scripts = folderScripts[entry.path] || new Set();
      const selectedPortVar = folderPortVars[entry.path] || null;
      const selectedPort = entry.ports.find(p => p.variable === selectedPortVar);

      const commands: RunCommand[] = [];
      scripts.forEach(scriptName => {
        const script = entry.scripts.find(s => s.name === scriptName);
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

      folders.push({
        id: crypto.randomUUID(),
        name: entry.name,
        folderPath: entry.path,
        commands,
        envFilePath: entry.hasEnv ? '.env' : undefined,
        portVariable: selectedPortVar || undefined,
        hidden: !isSelected,
      });
    }

    const project: Project = {
      id: crypto.randomUUID(),
      name: projectName,
      rootPath,
      folders,
      color,
      pinned: false,
      notes: '',
      createdAt: new Date().toISOString(),
      updatedAt: new Date().toISOString(),
    };

    await addProject(project);
    onClose();
  };

  return (
    <div className="fixed inset-0 bg-black/60 flex items-center justify-center z-50" onClick={onClose}>
      <div className="bg-zinc-800 rounded-lg border border-zinc-700 shadow-xl w-[540px] max-h-[80vh] overflow-hidden" onClick={e => e.stopPropagation()}>
        <div className="flex items-center justify-between p-4 border-b border-zinc-700">
          <h2 className="text-sm font-semibold text-zinc-100">
            Add Project {step === 1 ? '— Name & Color' : step === 2 ? '— Root Folder' : '— Configure Folders'}
          </h2>
          <button onClick={onClose} className="p-1 hover:bg-zinc-700 rounded text-zinc-400">
            <X size={16} />
          </button>
        </div>

        <div className="p-4 space-y-4 overflow-y-auto max-h-[60vh]">
          {step === 1 && (
            <>
              <div>
                <label className="text-xs text-zinc-400 mb-1 block">Project Name</label>
                <input
                  type="text"
                  value={projectName}
                  onChange={e => setProjectName(e.target.value)}
                  placeholder="My App"
                  className="w-full px-3 py-2 bg-zinc-900 border border-zinc-700 rounded text-xs text-zinc-100 focus:outline-none focus:border-indigo-500"
                  autoFocus
                />
              </div>
              <div>
                <label className="text-xs text-zinc-400 mb-1 block">Color</label>
                <div className="flex gap-2">
                  {PALETTE.map(c => (
                    <button
                      key={c}
                      onClick={() => setColor(c)}
                      className={`w-6 h-6 rounded-full border-2 ${color === c ? 'border-white' : 'border-transparent'}`}
                      style={{ backgroundColor: c }}
                    />
                  ))}
                </div>
              </div>
            </>
          )}

          {step === 2 && (
            <>
              <div>
                <label className="text-xs text-zinc-400 mb-1 block">Root Folder</label>
                <button
                  onClick={handlePickRoot}
                  className="w-full flex items-center gap-2 px-3 py-2 bg-zinc-900 border border-zinc-700 rounded text-xs text-zinc-300 hover:border-zinc-500"
                >
                  <FolderOpen size={14} />
                  {rootPath || 'Select root folder...'}
                </button>
                <p className="text-[10px] text-zinc-500 mt-1">
                  Sub-folders with package.json will be discovered automatically
                </p>
              </div>

              {scanning && (
                <div className="flex items-center gap-2 text-xs text-zinc-400">
                  <Loader2 size={14} className="animate-spin" />
                  Scanning for sub-projects...
                </div>
              )}

              {scanEntries.length > 0 && (
                <div>
                  <label className="text-xs text-zinc-400 mb-1 block">
                    Discovered folders ({scanEntries.length})
                  </label>
                  <div className="space-y-1 max-h-48 overflow-y-auto">
                    {scanEntries.map(entry => (
                      <div
                        key={entry.path}
                        className="flex items-center gap-2 px-2 py-1.5 rounded hover:bg-zinc-700/50 cursor-pointer"
                        onClick={() => toggleFolder(entry.path)}
                      >
                        <div className={`w-4 h-4 rounded border flex items-center justify-center ${
                          selectedFolders.has(entry.path) ? 'bg-indigo-600 border-indigo-600' : 'border-zinc-600'
                        }`}>
                          {selectedFolders.has(entry.path) && <Check size={12} className="text-white" />}
                        </div>
                        <span className="text-xs text-zinc-200 font-mono">{entry.name}</span>
                        <span className="text-[10px] text-zinc-500 ml-auto">
                          {entry.scripts.length} scripts{entry.hasEnv ? ' + .env' : ''}
                        </span>
                      </div>
                    ))}
                  </div>
                </div>
              )}

              {rootPath && !scanning && scanEntries.length === 0 && (
                <div className="text-xs text-amber-400 bg-amber-400/10 px-3 py-2 rounded">
                  No sub-folders with package.json found. You can still add folders manually after creating the project.
                </div>
              )}
            </>
          )}

          {step === 3 && (
            <>
              {scanEntries.filter(e => selectedFolders.has(e.path)).map(entry => (
                <div key={entry.path} className="space-y-2">
                  <div className="flex items-center gap-2">
                    <span className="text-xs font-medium text-zinc-200">{entry.name}</span>
                    <span className="text-[10px] text-zinc-500">{entry.path}</span>
                  </div>
                  <div className="ml-4 space-y-1">
                    {entry.scripts.map(script => (
                      <div
                        key={script.name}
                        className="flex items-center gap-2 px-2 py-1 rounded hover:bg-zinc-700/50 cursor-pointer"
                        onClick={() => toggleScript(entry.path, script.name)}
                      >
                        <div className={`w-3.5 h-3.5 rounded border flex items-center justify-center ${
                          (folderScripts[entry.path] || new Set()).has(script.name) ? 'bg-indigo-600 border-indigo-600' : 'border-zinc-600'
                        }`}>
                          {(folderScripts[entry.path] || new Set()).has(script.name) && <Check size={10} className="text-white" />}
                        </div>
                        <span className="text-xs text-zinc-300 font-mono">{script.name}</span>
                        <span className="text-[10px] text-zinc-500 truncate ml-auto">{script.command}</span>
                      </div>
                    ))}
                    {entry.scripts.length === 0 && (
                      <span className="text-[10px] text-zinc-500 px-2">No npm scripts found</span>
                    )}
                  </div>
                  {entry.ports.length > 0 && (
                    <div className="ml-4 mt-1">
                      <label className="text-[10px] text-zinc-500 mb-0.5 block">
                        Port Variable {entry.ports.length > 1 ? '(select one)' : ''}
                      </label>
                      <div className="space-y-0.5">
                        {entry.ports.map(p => (
                          <div
                            key={p.variable}
                            className={`flex items-center gap-2 px-2 py-1 rounded cursor-pointer ${
                              folderPortVars[entry.path] === p.variable ? 'bg-indigo-600/20 border border-indigo-500/50' : 'hover:bg-zinc-700/50 border border-transparent'
                            }`}
                            onClick={() => setFolderPortVars(prev => ({ ...prev, [entry.path]: p.variable }))}
                          >
                            <div className={`w-3 h-3 rounded-full border-2 flex items-center justify-center ${
                              folderPortVars[entry.path] === p.variable ? 'border-indigo-500' : 'border-zinc-600'
                            }`}>
                              {folderPortVars[entry.path] === p.variable && <div className="w-1.5 h-1.5 rounded-full bg-indigo-500" />}
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
              ))}
            </>
          )}
        </div>

        <div className="flex justify-between gap-2 p-4 border-t border-zinc-700">
          <div>
            {step > 1 && (
              <button
                onClick={() => setStep((step - 1) as 1 | 2 | 3)}
                className="flex items-center gap-1 px-4 py-1.5 text-xs text-zinc-400 hover:text-zinc-100"
              >
                <ArrowLeft size={12} /> Back
              </button>
            )}
          </div>
          <div className="flex gap-2">
            <button
              onClick={onClose}
              className="px-4 py-1.5 text-xs text-zinc-400 hover:text-zinc-100"
            >
              Cancel
            </button>
            {step === 1 && (
              <button
                onClick={() => setStep(2)}
                disabled={!projectName}
                className="flex items-center gap-1 px-4 py-1.5 bg-indigo-600 hover:bg-indigo-500 disabled:opacity-50 disabled:cursor-not-allowed text-white text-xs font-medium rounded"
              >
                Next <ArrowRight size={12} />
              </button>
            )}
            {step === 2 && (
              <button
                onClick={() => scanEntries.length > 0 ? setStep(3) : handleAdd()}
                disabled={!rootPath}
                className="flex items-center gap-1 px-4 py-1.5 bg-indigo-600 hover:bg-indigo-500 disabled:opacity-50 disabled:cursor-not-allowed text-white text-xs font-medium rounded"
              >
                {scanEntries.length > 0 ? <>Configure <ArrowRight size={12} /></> : 'Create Project'}
              </button>
            )}
            {step === 3 && (
              <button
                onClick={handleAdd}
                disabled={!projectName || !rootPath}
                className="px-4 py-1.5 bg-indigo-600 hover:bg-indigo-500 disabled:opacity-50 disabled:cursor-not-allowed text-white text-xs font-medium rounded"
              >
                Create Project
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
