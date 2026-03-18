import { useState, useRef, useEffect } from 'react';
import { Play, Square, Terminal, Pin, PinOff, Trash2, MoreVertical, GitBranch, ChevronDown, ChevronRight, AlertTriangle, FileText, FileCode, Download, FolderPlus, Folder, Pencil, ArrowUp, ArrowDown, Sparkles, Bot, X } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type { Project, ProjectFolder, DependencyStatus } from '../../types/config';
import { useAppStore, TabInfo } from '../../stores/appStore';
import { useProcessStore } from '../../stores/processStore';
import { useProcess } from '../../hooks/useProcess';
import { useConfig } from '../../hooks/useConfig';
import { usePty } from '../../hooks/usePty';
import { ensureSessionBuffer } from '../../utils/terminalBuffers';
import { ProjectNotes } from './ProjectNotes';
import { EnvEditor } from './EnvEditor';
import { AddFolderDialog } from './AddFolderDialog';
import { EditProjectDialog } from './EditProjectDialog';
import { EditFolderDialog } from './EditFolderDialog';
import { getAllCommands } from '../../utils/projectHelpers';
import { resolveInteractiveShellCommand } from '../../utils/runtimePlatform';
import { getSidebarTerminalLabel } from '../../utils/tabTitles';

function FolderSection({ project, folder }: { project: Project; folder: ProjectFolder }) {
  const [expanded, setExpanded] = useState(true);
  const [showMenu, setShowMenu] = useState(false);
  const [showEnvEditor, setShowEnvEditor] = useState(false);
  const [showEditFolder, setShowEditFolder] = useState(false);
  const [gitBranch, setGitBranch] = useState<string | null>(null);
  const [depStatus, setDepStatus] = useState<DependencyStatus | null>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const { openServerTab, updateProject } = useAppStore();
  const activeTabId = useAppStore(s => s.activeTabId);
  const processes = useProcessStore(s => s.processes);
  const { startProcess, stopProcess } = useProcess();
  const { getGitBranch, openTerminal, checkDependencies } = useConfig();

  useEffect(() => {
    getGitBranch(folder.folderPath).then(b => setGitBranch(b)).catch(() => {});
    checkDependencies(folder.folderPath).then(s => setDepStatus(s)).catch(() => {});
    // Listen for git branch changes from Rust file watcher (instant, no polling)
    const unlisten = listen<{ folder_path: string; branch: string | null }>('git-branch-changed', (event) => {
      if (event.payload.folder_path === folder.folderPath) {
        setGitBranch(event.payload.branch);
      }
    });
    return () => { unlisten.then(fn => fn()); };
  }, [folder.folderPath]);

  // Re-check dependencies when npm install finishes
  const installProc = processes[`${folder.id}-npm-install`];
  const installStatus = installProc?.status;
  useEffect(() => {
    if (installStatus === 'stopped' || installStatus === 'crashed') {
      checkDependencies(folder.folderPath).then(s => setDepStatus(s)).catch(() => {});
    }
  }, [installStatus]);

  useEffect(() => {
    const handleClick = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setShowMenu(false);
      }
    };
    if (showMenu) document.addEventListener('mousedown', handleClick);
    return () => document.removeEventListener('mousedown', handleClick);
  }, [showMenu]);

  const handleRemoveFolder = () => {
    const updated = {
      ...project,
      folders: project.folders.filter(f => f.id !== folder.id),
      updatedAt: new Date().toISOString(),
    };
    updateProject(updated);
    setShowMenu(false);
  };

  const singleCmd = folder.commands.length === 1 ? folder.commands[0] : null;
  const singleProc = singleCmd ? processes[singleCmd.id] : null;
  const singleStatus = singleProc?.status || 'stopped';
  const singleRunning = singleStatus === 'running' || singleStatus === 'starting';

  const contextMenu = showMenu && (
    <div className="absolute right-0 top-full mt-1 z-50 w-40 bg-zinc-800 border border-zinc-600 rounded-md shadow-lg py-1">
      <button onClick={() => { setShowEditFolder(true); setShowMenu(false); }} className="w-full text-left px-3 py-1.5 text-xs text-zinc-300 hover:bg-zinc-700 flex items-center gap-2">
        <Pencil size={12} /> Edit Folder
      </button>
      <button onClick={() => { openTerminal(folder.folderPath); setShowMenu(false); }} className="w-full text-left px-3 py-1.5 text-xs text-zinc-300 hover:bg-zinc-700 flex items-center gap-2">
        <Terminal size={12} /> Open Terminal
      </button>
      <button onClick={() => {
        const installCmdId = `${folder.id}-npm-install`;
        const installCmd = { id: installCmdId, label: 'npm install', command: 'npm', args: ['install'], clearLogsOnRestart: true };
        startProcess(folder, installCmd, project.id);
        setShowMenu(false);
      }} className="w-full text-left px-3 py-1.5 text-xs text-zinc-300 hover:bg-zinc-700 flex items-center gap-2">
        <Download size={12} /> npm install
      </button>
      {folder.envFilePath && (
        <button onClick={() => { setShowEnvEditor(true); setShowMenu(false); }} className="w-full text-left px-3 py-1.5 text-xs text-zinc-300 hover:bg-zinc-700 flex items-center gap-2">
          <FileCode size={12} /> Edit .env
        </button>
      )}
      <div className="border-t border-zinc-700 my-1" />
      <button onClick={handleRemoveFolder} className="w-full text-left px-3 py-1.5 text-xs text-red-400 hover:bg-zinc-700 flex items-center gap-2">
        <Trash2 size={12} /> Remove Folder
      </button>
    </div>
  );

  const dialogs = (
    <>
      {showEnvEditor && folder.envFilePath && (
        <EnvEditor
          filePath={`${folder.folderPath}/${folder.envFilePath}`}
          onClose={() => setShowEnvEditor(false)}
        />
      )}
      {showEditFolder && (
        <EditFolderDialog project={project} folder={folder} onClose={() => setShowEditFolder(false)} />
      )}
    </>
  );

  // Compact single-line view: folder name + status dot + command label + branch + port + play/stop + menu
  if (singleCmd) {
    const isActive = activeTabId === singleCmd.id;
    return (
      <div className="group/folder">
        <div
          className={`flex items-center gap-1.5 px-2 py-1 rounded cursor-pointer text-xs ${
            isActive ? 'bg-zinc-700/50' : 'hover:bg-zinc-700/30'
          }`}
          onClick={() => openServerTab(singleCmd.id, project.id)}
        >
          <div className={`w-1.5 h-1.5 rounded-full flex-shrink-0 ${
            singleRunning ? 'bg-emerald-400' :
            singleStatus === 'crashed' ? 'bg-red-400' :
            singleStatus === 'stopping' ? 'bg-amber-400' :
            'bg-zinc-600'
          }`} />
          <Folder size={11} className="text-zinc-500 flex-shrink-0" />
          <span className="text-[11px] font-medium text-zinc-300 truncate flex-shrink-0">{folder.name}</span>
          <span className="text-[10px] text-zinc-500 truncate hidden group-hover/folder:inline">{singleCmd.label}</span>
          {depStatus && depStatus.status !== 'ok' && (
            <span
              className={`flex-shrink-0 ${depStatus.status === 'missing' ? 'text-red-400' : 'text-amber-400'}`}
              title={depStatus.message}
            >
              <AlertTriangle size={10} />
            </span>
          )}
          {gitBranch && (
            <span className="flex items-center gap-0.5 text-[10px] text-zinc-500 flex-shrink-0">
              <GitBranch size={9} />
              <span className="max-w-[60px] truncate">{gitBranch}</span>
            </span>
          )}
          {singleCmd.port && (
            <span className="text-[10px] text-zinc-600 flex-shrink-0">:{singleCmd.port}</span>
          )}
          {!singleRunning ? (
            <button
              onClick={(e) => { e.stopPropagation(); startProcess(folder, singleCmd, project.id); }}
              className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-emerald-400 opacity-0 group-hover/folder:opacity-100 flex-shrink-0"
            >
              <Play size={12} />
            </button>
          ) : (
            <button
              onClick={(e) => { e.stopPropagation(); stopProcess(singleCmd.id); }}
              className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-red-400 opacity-0 group-hover/folder:opacity-100 flex-shrink-0"
            >
              <Square size={12} />
            </button>
          )}
          <div className="relative flex-shrink-0" ref={menuRef}>
            <button
              onClick={(e) => { e.stopPropagation(); setShowMenu(!showMenu); }}
              className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-zinc-300 opacity-0 group-hover/folder:opacity-100"
            >
              <MoreVertical size={12} />
            </button>
            {contextMenu}
          </div>
        </div>
        {dialogs}
      </div>
    );
  }

  // Multi-command: expandable folder with command children
  return (
    <div className="group/folder">
      <div className="flex items-center gap-1.5 px-2 py-1 rounded hover:bg-zinc-700/30 cursor-pointer">
        <button onClick={() => setExpanded(!expanded)} className="text-zinc-500">
          {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        </button>
        <Folder size={12} className="text-zinc-500 flex-shrink-0" />
        <div className="flex-1 min-w-0" onClick={() => setExpanded(!expanded)}>
          <div className="flex items-center gap-1">
            <span className="text-[11px] font-medium text-zinc-300 truncate">{folder.name}</span>
            {depStatus && depStatus.status !== 'ok' && (
              <span
                className={`flex-shrink-0 ${depStatus.status === 'missing' ? 'text-red-400' : 'text-amber-400'}`}
                title={depStatus.message}
              >
                <AlertTriangle size={10} />
              </span>
            )}
          </div>
          {gitBranch && (
            <div className="flex items-center gap-1 text-[10px] text-zinc-500">
              <GitBranch size={9} />
              <span className="truncate">{gitBranch}</span>
            </div>
          )}
        </div>
        <div className="relative" ref={menuRef}>
          <button
            onClick={(e) => { e.stopPropagation(); setShowMenu(!showMenu); }}
            className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-zinc-300 opacity-0 group-hover/folder:opacity-100"
          >
            <MoreVertical size={12} />
          </button>
          {contextMenu}
        </div>
      </div>

      {expanded && (
        <div className="ml-6 space-y-0.5">
          {folder.commands.map(cmd => {
            const proc = processes[cmd.id];
            const status = proc?.status || 'stopped';
            const isRunning = status === 'running' || status === 'starting';
            const isCmdActive = activeTabId === cmd.id;
            return (
              <div
                key={cmd.id}
                className={`flex items-center gap-1.5 px-2 py-1 rounded cursor-pointer text-xs ${
                  isCmdActive ? 'bg-zinc-700/50' : 'hover:bg-zinc-700/30'
                }`}
                onClick={() => openServerTab(cmd.id, project.id)}
              >
                <div className={`w-1.5 h-1.5 rounded-full ${
                  isRunning ? 'bg-emerald-400' :
                  status === 'crashed' ? 'bg-red-400' :
                  status === 'stopping' ? 'bg-amber-400' :
                  'bg-zinc-600'
                }`} />
                <span className="flex-1 text-zinc-400 truncate">{cmd.label}</span>
                {cmd.port && (
                  <span className="text-[10px] text-zinc-600">:{cmd.port}</span>
                )}
                {!isRunning ? (
                  <button
                    onClick={(e) => { e.stopPropagation(); startProcess(folder, cmd, project.id); }}
                    className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-emerald-400 opacity-0 group-hover/folder:opacity-100"
                  >
                    <Play size={12} />
                  </button>
                ) : (
                  <button
                    onClick={(e) => { e.stopPropagation(); stopProcess(cmd.id); }}
                    className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-red-400 opacity-0 group-hover/folder:opacity-100"
                  >
                    <Square size={12} />
                  </button>
                )}
              </div>
            );
          })}
        </div>
      )}

      {dialogs}
    </div>
  );
}

const DEFAULT_CLAUDE_CMD = 'npx -y @anthropic-ai/claude-code@latest --dangerously-skip-permissions';
const DEFAULT_CODEX_CMD = 'npx -y @openai/codex@latest --dangerously-bypass-approvals-and-sandbox';

function AITerminalList({ project, tabType, label, icon, iconColor, getCommand }: {
  project: Project;
  tabType: 'claude' | 'codex';
  label: string;
  icon: React.ReactNode;
  iconColor: string;
  getCommand: () => string;
}) {
  const openTabs = useAppStore(s => s.openTabs);
  const activeTabId = useAppStore(s => s.activeTabId);
  const { setActiveTab, closeTab } = useAppStore();
  const terminalActivity = useProcessStore(s => s.terminalActivity);
  const terminalTitles = useProcessStore(s => s.terminalTitles);
  const unseenReady = useProcessStore(s => s.unseenReady);
  const clearUnseenReady = useProcessStore(s => s.clearUnseenReady);
  const processes = useProcessStore(s => s.processes);
  const { createSession } = usePty();
  const config = useAppStore(s => s.config);
  const runtimeInfo = useAppStore(s => s.runtimeInfo);
  const openTab = useAppStore(s => s.openTab);

  const aiTabs = openTabs.filter(t => t.type === tabType && t.projectId === project.id);

  const launchAISession = async (sessionId: string) => {
    const shell = resolveInteractiveShellCommand(runtimeInfo, config?.settings ?? {
      theme: 'dark',
      logBufferSize: 10000,
      confirmOnClose: true,
      minimizeToTray: false,
      restoreSessionOnStart: true,
      defaultTerminal: 'bash',
      macTerminalProfile: 'system',
    });
    const cwd = project.rootPath;

    // Pre-create session buffer with activity tracking so spinner detection starts immediately
    ensureSessionBuffer(sessionId, undefined, true);

    await createSession(sessionId, cwd, shell.command, shell.args);

    // Write AI startup command after short delay for shell init
    const aiCmd = getCommand();
    setTimeout(async () => {
      try {
        await invoke('write_pty', { id: sessionId, data: aiCmd + '\r\n' });
      } catch {
        // Session may have closed
      }
    }, 500);
  };

  const handleClick = async (tab: TabInfo) => {
    const proc = processes[tab.ptySessionId || ''];
    const isAlive = proc?.status === 'running' || proc?.status === 'starting';
    if (!isAlive) {
      // Stopped — relaunch a fresh session
      const newSessionId = crypto.randomUUID();
      try {
        await launchAISession(newSessionId);

        // Update tab with new session ID
        openTab({
          ...tab,
          ptySessionId: newSessionId,
        });
      } catch (err) {
        console.error(`Failed to relaunch ${label}:`, err);
      }
    } else {
      // Clear "ready" indicator when visiting
      if (tab.ptySessionId) clearUnseenReady(tab.ptySessionId);
      setActiveTab(tab.id);
    }
  };

  return (
    <>
      {aiTabs.map(tab => {
        const sessionId = tab.ptySessionId || '';
        const activity = terminalActivity[sessionId];
        const proc = processes[sessionId];
        const isRunning = proc?.status === 'running';
        const isReady = unseenReady[sessionId];
        const displayLabel = getSidebarTerminalLabel(tab, config, terminalTitles);

        return (
          <div
            key={tab.id}
            className={`group/ai flex items-center gap-1.5 px-2 py-1 rounded cursor-pointer text-xs ${
              activeTabId === tab.id ? 'bg-zinc-700/50' : 'hover:bg-zinc-700/30'
            }`}
            onClick={() => handleClick(tab)}
          >
            <span className={
              isReady
                ? 'text-emerald-400'
                : activity === 'thinking'
                ? 'text-amber-400 animate-pulse'
                : iconColor
            }>{icon}</span>
            <span className="flex-1 text-zinc-400 truncate">{displayLabel}</span>
            {isReady && (
              <span className="w-1.5 h-1.5 rounded-full bg-emerald-400 animate-pulse flex-shrink-0" />
            )}
            <span className={`text-[10px] ${
              isReady ? 'text-emerald-400' :
              activity === 'thinking' ? 'text-amber-400' :
              isRunning ? 'text-zinc-500' : 'text-zinc-600'
            }`}>
              {isReady ? 'ready' : activity === 'thinking' ? 'thinking' : isRunning ? 'idle' : 'stopped'}
            </span>
            <button
              onClick={(e) => { e.stopPropagation(); closeTab(tab.id); }}
              className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-zinc-300 opacity-0 group-hover/ai:opacity-100"
            >
              <X size={10} />
            </button>
          </div>
        );
      })}
    </>
  );
}

function AILaunchButton({ icon, iconColor, label, onClick }: {
  icon: React.ReactNode;
  iconColor: string;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      onClick={onClick}
      className={`flex items-center gap-1 px-2 py-1 rounded hover:bg-zinc-700/30 text-xs ${iconColor} hover:opacity-80`}
    >
      {icon}
      <span>+ {label}</span>
    </button>
  );
}

function useAILauncher(project: Project, tabType: 'claude' | 'codex', label: string, getCommand: () => string) {
  const openTabs = useAppStore(s => s.openTabs);
  const config = useAppStore(s => s.config);
  const runtimeInfo = useAppStore(s => s.runtimeInfo);
  const openTab = useAppStore(s => s.openTab);
  const { createSession } = usePty();

  const aiTabs = openTabs.filter(t => t.type === tabType && t.projectId === project.id);

  const launchAISession = async (sessionId: string) => {
    const shell = resolveInteractiveShellCommand(runtimeInfo, config?.settings ?? {
      theme: 'dark',
      logBufferSize: 10000,
      confirmOnClose: true,
      minimizeToTray: false,
      restoreSessionOnStart: true,
      defaultTerminal: 'bash',
      macTerminalProfile: 'system',
    });
    const cwd = project.rootPath;

    ensureSessionBuffer(sessionId, undefined, true);
    await createSession(sessionId, cwd, shell.command, shell.args);

    const aiCmd = getCommand();
    setTimeout(async () => {
      try {
        await invoke('write_pty', { id: sessionId, data: aiCmd + '\r\n' });
      } catch {}
    }, 500);
  };

  const handleLaunch = async () => {
    const nextNum = aiTabs.length + 1;
    const sessionId = crypto.randomUUID();
    const tabLabel = `${label} ${nextNum}`;

    try {
      await launchAISession(sessionId);
      openTab({
        id: sessionId,
        type: tabType,
        projectId: project.id,
        ptySessionId: sessionId,
        label: tabLabel,
      });
    } catch (err) {
      console.error(`Failed to launch ${label}:`, err);
    }
  };

  return handleLaunch;
}

function AILaunchButtons({ project }: { project: Project }) {
  const config = useAppStore(s => s.config);
  const launchClaude = useAILauncher(project, 'claude', 'Claude',
    () => config?.settings.claudeCommand || DEFAULT_CLAUDE_CMD);
  const launchCodex = useAILauncher(project, 'codex', 'Codex',
    () => config?.settings.codexCommand || DEFAULT_CODEX_CMD);

  return (
    <div className="flex items-center gap-0.5">
      <AILaunchButton icon={<Sparkles size={11} />} iconColor="text-purple-400" label="Claude" onClick={launchClaude} />
      <AILaunchButton icon={<Bot size={11} />} iconColor="text-emerald-400" label="Codex" onClick={launchCodex} />
    </div>
  );
}

export function ProjectCard({ project }: { project: Project }) {
  const [expanded, setExpanded] = useState(true);
  const [showMenu, setShowMenu] = useState(false);
  const [showNotes, setShowNotes] = useState(false);
  const [showAddFolder, setShowAddFolder] = useState(false);
  const [showEditProject, setShowEditProject] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);
  const { updateProject, removeProject, reorderProjects, config } = useAppStore();
  const processes = useProcessStore(s => s.processes);
  const { startAllForProject, stopAllForProject } = useProcess();

  const allCommands = getAllCommands(project);
  const runningCount = allCommands.filter(
    cmd => processes[cmd.id]?.status === 'running'
  ).length;

  useEffect(() => {
    const handleClick = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setShowMenu(false);
      }
    };
    if (showMenu) document.addEventListener('mousedown', handleClick);
    return () => document.removeEventListener('mousedown', handleClick);
  }, [showMenu]);

  const handleTogglePin = () => {
    updateProject({ ...project, pinned: !project.pinned, updatedAt: new Date().toISOString() });
    setShowMenu(false);
  };

  const handleRemove = () => {
    if (runningCount > 0) {
      stopAllForProject(project.id).then(() => removeProject(project.id));
    } else {
      removeProject(project.id);
    }
    setShowMenu(false);
  };

  const projects = config?.projects ?? [];
  const projectIndex = projects.findIndex(p => p.id === project.id);
  const canMoveUp = projectIndex > 0;
  const canMoveDown = projectIndex < projects.length - 1;

  const handleMove = (direction: 'up' | 'down') => {
    const ids = projects.map(p => p.id);
    const i = ids.indexOf(project.id);
    const j = direction === 'up' ? i - 1 : i + 1;
    [ids[i], ids[j]] = [ids[j], ids[i]];
    reorderProjects(ids);
  };

  const visibleFolders = project.folders.filter(f => !f.hidden);

  return (
    <div className="group">
      <div className="flex items-center gap-1.5 px-2 py-1.5 rounded hover:bg-zinc-700/50 cursor-pointer">
        <button onClick={() => setExpanded(!expanded)} className="text-zinc-500">
          {expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
        </button>
        <div
          className="w-2 h-2 rounded-full flex-shrink-0"
          style={{ backgroundColor: project.color || '#6366f1' }}
        />
        <div className="flex-1 min-w-0" onClick={() => setExpanded(!expanded)}>
          <span className="text-xs font-medium text-zinc-200 truncate">{project.name}</span>
        </div>
        <div className="flex items-center opacity-0 group-hover:opacity-100 flex-shrink-0">
          <button
            onClick={(e) => { e.stopPropagation(); setShowNotes(true); }}
            className={`p-0.5 rounded hover:bg-zinc-600 hover:text-zinc-300 ${project.notes ? 'text-amber-400/70' : 'text-zinc-500'}`}
            title="Notes"
          >
            <FileText size={12} />
          </button>
          <button
            onClick={(e) => { e.stopPropagation(); handleMove('up'); }}
            disabled={!canMoveUp}
            className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-zinc-300 disabled:opacity-0 disabled:pointer-events-none"
          >
            <ArrowUp size={12} />
          </button>
          <button
            onClick={(e) => { e.stopPropagation(); handleMove('down'); }}
            disabled={!canMoveDown}
            className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-zinc-300 disabled:opacity-0 disabled:pointer-events-none"
          >
            <ArrowDown size={12} />
          </button>
        </div>
        {runningCount > 0 && (
          <span className="text-[10px] bg-emerald-600/20 text-emerald-400 px-1.5 rounded-full">
            {runningCount}
          </span>
        )}
        <div className="relative" ref={menuRef}>
          <button
            onClick={(e) => { e.stopPropagation(); setShowMenu(!showMenu); }}
            className="p-1 rounded hover:bg-zinc-600 text-zinc-500 hover:text-zinc-300 opacity-0 group-hover:opacity-100"
          >
            <MoreVertical size={14} />
          </button>
          {showMenu && (
            <div className="absolute right-0 top-full mt-1 z-50 w-44 bg-zinc-800 border border-zinc-600 rounded-md shadow-lg py-1">
              <button onClick={() => { startAllForProject(project.id); setShowMenu(false); }} className="w-full text-left px-3 py-1.5 text-xs text-zinc-300 hover:bg-zinc-700 flex items-center gap-2">
                <Play size={12} /> Start All
              </button>
              <button onClick={() => { stopAllForProject(project.id); setShowMenu(false); }} className="w-full text-left px-3 py-1.5 text-xs text-zinc-300 hover:bg-zinc-700 flex items-center gap-2">
                <Square size={12} /> Stop All
              </button>
              <button onClick={() => { setShowAddFolder(true); setShowMenu(false); }} className="w-full text-left px-3 py-1.5 text-xs text-zinc-300 hover:bg-zinc-700 flex items-center gap-2">
                <FolderPlus size={12} /> Add Folder
              </button>
              <div className="border-t border-zinc-700 my-1" />
              <button onClick={() => { setShowEditProject(true); setShowMenu(false); }} className="w-full text-left px-3 py-1.5 text-xs text-zinc-300 hover:bg-zinc-700 flex items-center gap-2">
                <Pencil size={12} /> Edit Project
              </button>
              <button onClick={() => { setShowNotes(true); setShowMenu(false); }} className="w-full text-left px-3 py-1.5 text-xs text-zinc-300 hover:bg-zinc-700 flex items-center gap-2">
                <FileText size={12} /> Notes
              </button>
              <button onClick={handleTogglePin} className="w-full text-left px-3 py-1.5 text-xs text-zinc-300 hover:bg-zinc-700 flex items-center gap-2">
                {project.pinned ? <><PinOff size={12} /> Unpin</> : <><Pin size={12} /> Pin to Top</>}
              </button>
              <div className="border-t border-zinc-700 my-1" />
              <button onClick={handleRemove} className="w-full text-left px-3 py-1.5 text-xs text-red-400 hover:bg-zinc-700 flex items-center gap-2">
                <Trash2 size={12} /> Remove Project
              </button>
            </div>
          )}
        </div>
      </div>

      {expanded && (
        <div className="ml-5 space-y-0.5">
          {visibleFolders.map(folder => (
            <FolderSection key={folder.id} project={project} folder={folder} />
          ))}
          <AITerminalList project={project} tabType="claude" label="Claude"
            icon={<Sparkles size={11} />} iconColor="text-purple-400"
            getCommand={() => config?.settings.claudeCommand || DEFAULT_CLAUDE_CMD} />
          <AITerminalList project={project} tabType="codex" label="Codex"
            icon={<Bot size={11} />} iconColor="text-emerald-400"
            getCommand={() => config?.settings.codexCommand || DEFAULT_CODEX_CMD} />
          <AILaunchButtons project={project} />
        </div>
      )}

      {showNotes && <ProjectNotes project={project} onClose={() => setShowNotes(false)} />}
      {showAddFolder && <AddFolderDialog projectId={project.id} onClose={() => setShowAddFolder(false)} />}
      {showEditProject && <EditProjectDialog project={project} onClose={() => setShowEditProject(false)} />}
    </div>
  );
}
