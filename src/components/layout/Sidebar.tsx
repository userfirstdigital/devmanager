import { useState } from 'react';
import { PanelLeftClose, PanelLeft, Plus, Square, Settings } from 'lucide-react';
import { useAppStore } from '../../stores/appStore';
import { ProjectList } from '../projects/ProjectList';
import { AddProjectDialog } from '../projects/AddProjectDialog';
import { SettingsDialog } from '../settings/SettingsDialog';
import { SSHList } from '../ssh/SSHList';
import { useProcess } from '../../hooks/useProcess';

export function Sidebar() {
  const { sidebarCollapsed, toggleSidebar } = useAppStore();
  const [showAddProject, setShowAddProject] = useState(false);
  const [showSettings, setShowSettings] = useState(false);
  const { stopAll } = useProcess();

  if (sidebarCollapsed) {
    return (
      <div className="w-12 bg-zinc-800 border-r border-zinc-700 flex flex-col items-center py-2 gap-2">
        <button
          onClick={toggleSidebar}
          className="p-2 hover:bg-zinc-700 rounded text-zinc-400 hover:text-zinc-100"
          title="Expand sidebar"
        >
          <PanelLeft size={18} />
        </button>
        <button
          onClick={() => setShowAddProject(true)}
          className="p-2 hover:bg-zinc-700 rounded text-zinc-400 hover:text-zinc-100"
          title="Add project"
        >
          <Plus size={18} />
        </button>
        <div className="flex-1" />
        <button
          onClick={() => setShowSettings(true)}
          className="p-2 hover:bg-zinc-700 rounded text-zinc-400 hover:text-zinc-100"
          title="Settings"
        >
          <Settings size={18} />
        </button>
        {showSettings && <SettingsDialog onClose={() => setShowSettings(false)} />}
      </div>
    );
  }

  return (
    <>
      <div className="w-60 bg-zinc-800 border-r border-zinc-700 flex flex-col">
        <div className="flex items-center justify-between p-3 border-b border-zinc-700">
          <div className="flex items-baseline gap-2 min-w-0">
            <h1 className="text-sm font-bold text-zinc-100 tracking-wide uppercase">DevManager</h1>
            <span className="text-[10px] font-medium text-zinc-500 shrink-0">v{__APP_VERSION__}</span>
          </div>
          <button
            onClick={toggleSidebar}
            className="p-1 hover:bg-zinc-700 rounded text-zinc-400 hover:text-zinc-100"
            title="Collapse sidebar"
          >
            <PanelLeftClose size={18} />
          </button>
        </div>

        <div className="flex-1 overflow-y-auto">
          <ProjectList />
          <div className="border-t border-zinc-700 mt-1">
            <SSHList />
          </div>
        </div>

        <div className="border-t border-zinc-700 p-2 flex gap-2">
          <button
            onClick={() => setShowAddProject(true)}
            className="flex-1 flex items-center justify-center gap-1.5 px-3 py-1.5 bg-indigo-600 hover:bg-indigo-500 text-white text-xs font-medium rounded"
          >
            <Plus size={14} />
            Add Project
          </button>
          <button
            onClick={stopAll}
            className="px-3 py-1.5 bg-zinc-700 hover:bg-red-600 text-zinc-300 hover:text-white text-xs font-medium rounded"
            title="Stop All Servers"
          >
            <Square size={14} />
          </button>
          <button
            onClick={() => setShowSettings(true)}
            className="px-3 py-1.5 bg-zinc-700 hover:bg-zinc-600 text-zinc-300 hover:text-white text-xs font-medium rounded"
            title="Settings"
          >
            <Settings size={14} />
          </button>
        </div>
      </div>

      {showAddProject && <AddProjectDialog onClose={() => setShowAddProject(false)} />}
      {showSettings && <SettingsDialog onClose={() => setShowSettings(false)} />}
    </>
  );
}
