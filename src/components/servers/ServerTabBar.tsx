import { useAppStore } from '../../stores/appStore';
import { useProcessStore } from '../../stores/processStore';
import { X, Sparkles, Bot, Terminal } from 'lucide-react';

export function ServerTabBar() {
  const { openTabs, activeTabId, setActiveTab, closeTab, config } = useAppStore();
  const processes = useProcessStore(s => s.processes);
  const resources = useProcessStore(s => s.resources);
  const terminalActivity = useProcessStore(s => s.terminalActivity);

  if (openTabs.length === 0) return null;

  const getServerLabel = (commandId: string, projectId: string) => {
    const project = config?.projects.find(p => p.id === projectId);
    let commandLabel = '?';
    let folderName = '';
    if (project) {
      for (const folder of project.folders) {
        const cmd = folder.commands.find(c => c.id === commandId);
        if (cmd) {
          commandLabel = cmd.label;
          folderName = project.folders.length > 1 ? folder.name : '';
          break;
        }
      }
    }
    const displayLabel = folderName ? `${folderName}/${commandLabel}` : commandLabel;
    return { projectName: project?.name || '?', commandLabel: displayLabel, projectColor: project?.color || '#6366f1' };
  };

  const getProjectInfo = (projectId: string) => {
    const project = config?.projects.find(p => p.id === projectId);
    return { projectName: project?.name || '?', projectColor: project?.color || '#6366f1' };
  };

  return (
    <div className="flex items-center bg-zinc-800 border-b border-zinc-700 overflow-x-auto">
      {openTabs.map(tab => {
        const isActive = activeTabId === tab.id;

        if (tab.type === 'server') {
          const { projectName, commandLabel, projectColor } = getServerLabel(tab.commandId!, tab.projectId);
          const proc = processes[tab.commandId!];
          const status = proc?.status || 'stopped';
          const res = resources[tab.commandId!];
          const memoryStr = res ? `${Math.round(res.total_memory_mb)} MB` : '';
          const errorCount = proc?.unseenErrorCount || 0;

          return (
            <div
              key={tab.id}
              className={`group flex items-center gap-1.5 px-3 py-2 cursor-pointer border-r border-zinc-700 min-w-0 max-w-48 ${
                isActive ? 'bg-zinc-900' : 'bg-zinc-800 hover:bg-zinc-750'
              }`}
              style={{ borderBottom: isActive ? `2px solid ${projectColor}` : '2px solid transparent' }}
              onClick={() => setActiveTab(tab.id)}
            >
              <div className={`w-1.5 h-1.5 rounded-full flex-shrink-0 ${
                status === 'running' ? 'bg-emerald-400' :
                status === 'crashed' ? 'bg-red-400' :
                status === 'starting' || status === 'stopping' ? 'bg-amber-400 animate-pulse' :
                'bg-zinc-600'
              }`} />
              <div className="min-w-0 flex-1">
                <div className="text-[11px] text-zinc-500 truncate">{projectName}</div>
                <div className="text-xs text-zinc-200 truncate">{commandLabel}</div>
              </div>
              {memoryStr && status === 'running' && (
                <span className="text-[10px] text-zinc-500 flex-shrink-0">{memoryStr}</span>
              )}
              {errorCount > 0 && !isActive && (
                <span className="text-[10px] bg-red-500 text-white px-1 rounded-full flex-shrink-0 min-w-[16px] text-center">
                  {errorCount > 99 ? '99+' : errorCount}
                </span>
              )}
              <button
                onClick={(e) => { e.stopPropagation(); closeTab(tab.id); }}
                className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-zinc-200 opacity-0 group-hover:opacity-100 flex-shrink-0"
              >
                <X size={12} />
              </button>
            </div>
          );
        }

        if (tab.type === 'claude') {
          const { projectName, projectColor } = getProjectInfo(tab.projectId);
          const activity = terminalActivity[tab.ptySessionId || ''];
          const res = resources[tab.ptySessionId || ''];
          const memoryStr = res ? `${Math.round(res.total_memory_mb)} MB` : '';

          return (
            <div
              key={tab.id}
              className={`group flex items-center gap-1.5 px-3 py-2 cursor-pointer border-r border-zinc-700 min-w-0 max-w-48 ${
                isActive ? 'bg-zinc-900' : 'bg-zinc-800 hover:bg-zinc-750'
              }`}
              style={{ borderBottom: isActive ? `2px solid ${projectColor}` : '2px solid transparent' }}
              onClick={() => setActiveTab(tab.id)}
            >
              <Sparkles size={12} className={
                activity === 'thinking'
                  ? 'text-amber-400 animate-pulse flex-shrink-0'
                  : 'text-purple-400 flex-shrink-0'
              } />
              <div className="min-w-0 flex-1">
                <div className="text-[11px] text-zinc-500 truncate">{projectName}</div>
                <div className="text-xs text-zinc-200 truncate">{tab.label || 'Claude'}</div>
              </div>
              {memoryStr && (
                <span className="text-[10px] text-zinc-500 flex-shrink-0">{memoryStr}</span>
              )}
              <button
                onClick={(e) => { e.stopPropagation(); closeTab(tab.id); }}
                className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-zinc-200 opacity-0 group-hover:opacity-100 flex-shrink-0"
              >
                <X size={12} />
              </button>
            </div>
          );
        }

        if (tab.type === 'codex') {
          const { projectName, projectColor } = getProjectInfo(tab.projectId);
          const activity = terminalActivity[tab.ptySessionId || ''];
          const res = resources[tab.ptySessionId || ''];
          const memoryStr = res ? `${Math.round(res.total_memory_mb)} MB` : '';

          return (
            <div
              key={tab.id}
              className={`group flex items-center gap-1.5 px-3 py-2 cursor-pointer border-r border-zinc-700 min-w-0 max-w-48 ${
                isActive ? 'bg-zinc-900' : 'bg-zinc-800 hover:bg-zinc-750'
              }`}
              style={{ borderBottom: isActive ? `2px solid ${projectColor}` : '2px solid transparent' }}
              onClick={() => setActiveTab(tab.id)}
            >
              <Bot size={12} className={
                activity === 'thinking'
                  ? 'text-amber-400 animate-pulse flex-shrink-0'
                  : 'text-emerald-400 flex-shrink-0'
              } />
              <div className="min-w-0 flex-1">
                <div className="text-[11px] text-zinc-500 truncate">{projectName}</div>
                <div className="text-xs text-zinc-200 truncate">{tab.label || 'Codex'}</div>
              </div>
              {memoryStr && (
                <span className="text-[10px] text-zinc-500 flex-shrink-0">{memoryStr}</span>
              )}
              <button
                onClick={(e) => { e.stopPropagation(); closeTab(tab.id); }}
                className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-zinc-200 opacity-0 group-hover:opacity-100 flex-shrink-0"
              >
                <X size={12} />
              </button>
            </div>
          );
        }

        if (tab.type === 'ssh') {
          const sshConn = config?.sshConnections?.find(c => c.id === tab.sshConnectionId);
          const res = resources[tab.ptySessionId || ''];
          const memoryStr = res ? `${Math.round(res.total_memory_mb)} MB` : '';

          return (
            <div
              key={tab.id}
              className={`group flex items-center gap-1.5 px-3 py-2 cursor-pointer border-r border-zinc-700 min-w-0 max-w-48 ${
                isActive ? 'bg-zinc-900' : 'bg-zinc-800 hover:bg-zinc-750'
              }`}
              style={{ borderBottom: isActive ? `2px solid #06b6d4` : '2px solid transparent' }}
              onClick={() => setActiveTab(tab.id)}
            >
              <Terminal size={12} className="text-cyan-400 flex-shrink-0" />
              <div className="min-w-0 flex-1">
                <div className="text-[11px] text-zinc-500 truncate">SSH</div>
                <div className="text-xs text-zinc-200 truncate">{sshConn?.label || tab.label || 'SSH'}</div>
              </div>
              {memoryStr && (
                <span className="text-[10px] text-zinc-500 flex-shrink-0">{memoryStr}</span>
              )}
              <button
                onClick={(e) => { e.stopPropagation(); closeTab(tab.id); }}
                className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-zinc-200 opacity-0 group-hover:opacity-100 flex-shrink-0"
              >
                <X size={12} />
              </button>
            </div>
          );
        }

        return null;
      })}
    </div>
  );
}
