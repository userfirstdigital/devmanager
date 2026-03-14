import { useState, useCallback } from 'react';
import { Search, Copy, Download, Trash2, ArrowDown, AlertCircle, AlertTriangle } from 'lucide-react';
import { save } from '@tauri-apps/plugin-dialog';
import { writeTextFile } from '@tauri-apps/plugin-fs';
import { useProcessStore } from '../../stores/processStore';
import { useAppStore } from '../../stores/appStore';

export function LogToolbar({ commandId }: { commandId: string }) {
  const [searchText, setSearchText] = useState('');
  const [searchVisible, setSearchVisible] = useState(false);
  const [highlightError, setHighlightError] = useState(false);
  const [highlightWarn, setHighlightWarn] = useState(false);
  const clearLogs = useProcessStore(s => s.clearLogs);
  const proc = useProcessStore(s => s.processes[commandId]);
  const config = useAppStore(s => s.config);
  const tab = useAppStore(s => s.openTabs.find(t => t.type === 'server' && t.commandId === commandId));

  const handleSearch = (direction: 'next' | 'prev' = 'next') => {
    const containers = document.querySelectorAll('.xterm');
    for (const container of containers) {
      const searchAddon = (container.parentElement as any)?.__searchAddon;
      if (searchAddon && searchText) {
        if (direction === 'next') {
          searchAddon.findNext(searchText);
        } else {
          searchAddon.findPrevious(searchText);
        }
        break;
      }
    }
  };

  const handleCopy = async () => {
    const logs = proc?.logs ?? [];
    const text = logs.map(l => l.replace(/\x1b\[[0-9;]*m/g, '')).join('\n');
    await navigator.clipboard.writeText(text);
  };

  const handleSave = async () => {
    const project = config?.projects.find(p => p.id === tab?.projectId);
    let commandLabel = 'output';
    if (project) {
      for (const folder of project.folders) {
        const cmd = folder.commands.find(c => c.id === commandId);
        if (cmd) { commandLabel = cmd.label; break; }
      }
    }
    const defaultName = `${project?.name || 'log'}-${commandLabel}-${new Date().toISOString().slice(0, 19).replace(/:/g, '-')}.txt`;

    const path = await save({
      defaultPath: defaultName,
      filters: [{ name: 'Text', extensions: ['txt'] }],
    });

    if (path) {
      const logs = proc?.logs ?? [];
      const text = logs.map(l => l.replace(/\x1b\[[0-9;]*m/g, '')).join('\n');
      await writeTextFile(path, text);
    }
  };

  const handleHighlight = useCallback((type: 'error' | 'warn', active: boolean) => {
    const containers = document.querySelectorAll('.xterm');
    for (const container of containers) {
      const searchAddon = (container.parentElement as any)?.__searchAddon;
      if (searchAddon) {
        if (active) {
          const pattern = type === 'error' ? 'error|Error|ERR|FATAL|fatal|panic|PANIC|exception|Exception' : 'warn|Warn|WARN|warning|WARNING';
          searchAddon.findNext(pattern, {
            regex: true,
            decorations: {
              matchBackground: type === 'error' ? '#7f1d1d' : '#78350f',
              matchOverviewRuler: type === 'error' ? '#ef4444' : '#f59e0b',
              activeMatchBackground: type === 'error' ? '#991b1b' : '#92400e',
              activeMatchColorOverviewRuler: type === 'error' ? '#f87171' : '#fbbf24',
            },
          });
        } else {
          searchAddon.clearDecorations();
        }
        break;
      }
    }
  }, []);

  const handleScrollToBottom = () => {
    const containers = document.querySelectorAll('.xterm');
    for (const container of containers) {
      const terminal = (container.parentElement as any)?.__terminal;
      if (terminal) {
        terminal.scrollToBottom();
        break;
      }
    }
  };

  return (
    <div className="flex items-center gap-2 px-3 py-1.5 bg-zinc-800 border-t border-zinc-700">
      <div className="flex items-center gap-1 flex-1">
        {searchVisible ? (
          <div className="flex items-center gap-1">
            <Search size={14} className="text-zinc-500" />
            <input
              type="text"
              value={searchText}
              onChange={e => setSearchText(e.target.value)}
              onKeyDown={e => {
                if (e.key === 'Enter') handleSearch(e.shiftKey ? 'prev' : 'next');
                if (e.key === 'Escape') { setSearchVisible(false); setSearchText(''); }
              }}
              placeholder="Search logs..."
              className="bg-zinc-900 border border-zinc-700 rounded px-2 py-0.5 text-xs text-zinc-100 w-48 focus:outline-none focus:border-indigo-500"
              autoFocus
            />
          </div>
        ) : (
          <button
            onClick={() => setSearchVisible(true)}
            className="p-1 rounded hover:bg-zinc-700 text-zinc-400 hover:text-zinc-200"
            title="Search (Ctrl+F)"
          >
            <Search size={14} />
          </button>
        )}
      </div>
      <div className="flex items-center gap-0.5 border-r border-zinc-700 pr-2 mr-1">
        <button
          onClick={() => {
            const next = !highlightError;
            setHighlightError(next);
            handleHighlight('error', next);
          }}
          className={`flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] font-medium ${
            highlightError ? 'bg-red-900/60 text-red-300' : 'text-zinc-500 hover:text-zinc-300 hover:bg-zinc-700'
          }`}
          title="Highlight errors"
        >
          <AlertCircle size={12} />
          ERR
        </button>
        <button
          onClick={() => {
            const next = !highlightWarn;
            setHighlightWarn(next);
            handleHighlight('warn', next);
          }}
          className={`flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] font-medium ${
            highlightWarn ? 'bg-amber-900/60 text-amber-300' : 'text-zinc-500 hover:text-zinc-300 hover:bg-zinc-700'
          }`}
          title="Highlight warnings"
        >
          <AlertTriangle size={12} />
          WARN
        </button>
      </div>
      <button onClick={handleScrollToBottom} className="p-1 rounded hover:bg-zinc-700 text-zinc-400 hover:text-zinc-200" title="Scroll to bottom">
        <ArrowDown size={14} />
      </button>
      <button onClick={handleCopy} className="p-1 rounded hover:bg-zinc-700 text-zinc-400 hover:text-zinc-200" title="Copy all logs">
        <Copy size={14} />
      </button>
      <button onClick={handleSave} className="p-1 rounded hover:bg-zinc-700 text-zinc-400 hover:text-zinc-200" title="Save logs to file">
        <Download size={14} />
      </button>
      <button onClick={() => clearLogs(commandId)} className="p-1 rounded hover:bg-zinc-700 text-zinc-400 hover:text-red-400" title="Clear logs">
        <Trash2 size={14} />
      </button>
    </div>
  );
}
