import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';
import type { AppConfig, Project, Settings, SessionState, TabType, SSHConnection, RuntimePlatformInfo } from '../types/config';
import { cleanupSessionBuffer } from '../utils/terminalBuffers';
import { useProcessStore } from './processStore';

export interface TabInfo {
  id: string;
  type: TabType;
  projectId: string;
  commandId?: string;       // server tabs
  ptySessionId?: string;    // claude + ssh tabs
  sshConnectionId?: string; // ssh tabs
  label?: string;           // display label for claude/ssh
}

interface AppState {
  config: AppConfig | null;
  runtimeInfo: RuntimePlatformInfo | null;
  activeTabId: string | null;
  openTabs: TabInfo[];
  sidebarCollapsed: boolean;
  loading: boolean;

  // Config actions
  loadConfig: () => Promise<void>;
  saveConfig: (config: AppConfig) => Promise<void>;
  addProject: (project: Project) => Promise<void>;
  updateProject: (project: Project) => Promise<void>;
  removeProject: (projectId: string) => Promise<void>;
  reorderProjects: (projectIds: string[]) => Promise<void>;
  updateSettings: (settings: Settings) => Promise<void>;

  // SSH actions
  addSSHConnection: (conn: SSHConnection) => Promise<void>;
  removeSSHConnection: (connId: string) => Promise<void>;
  updateSSHConnection: (conn: SSHConnection) => Promise<void>;

  // Tab actions
  setActiveTab: (tabId: string | null) => void;
  openTab: (tab: TabInfo) => void;
  openServerTab: (commandId: string, projectId: string) => void;
  closeTab: (tabId: string) => void;
  reorderTabs: (tabs: TabInfo[]) => void;

  // Sidebar
  toggleSidebar: () => void;
  setSidebarCollapsed: (collapsed: boolean) => void;

  // Session
  loadSession: () => Promise<void>;
  saveSession: () => Promise<void>;
}

export const useAppStore = create<AppState>((set, get) => ({
  config: null,
  runtimeInfo: null,
  activeTabId: null,
  openTabs: [],
  sidebarCollapsed: false,
  loading: true,

  loadConfig: async () => {
    try {
      const [config, runtimeInfo] = await Promise.all([
        invoke<AppConfig>('get_config'),
        invoke<RuntimePlatformInfo>('get_runtime_info').catch(err => {
          console.warn('Failed to load runtime info:', err);
          return null;
        }),
      ]);
      set({ config, runtimeInfo, loading: false });
    } catch (err) {
      console.error('Failed to load config:', err);
      set({ loading: false });
    }
  },

  saveConfig: async (config: AppConfig) => {
    try {
      await invoke('save_full_config', { config });
      set({ config });
    } catch (err) {
      console.error('Failed to save config:', err);
    }
  },

  addProject: async (project: Project) => {
    try {
      const config = await invoke<AppConfig>('add_project', { project });
      set({ config });
    } catch (err) {
      console.error('Failed to add project:', err);
    }
  },

  updateProject: async (project: Project) => {
    try {
      const config = await invoke<AppConfig>('update_project', { project });
      set({ config });
    } catch (err) {
      console.error('Failed to update project:', err);
    }
  },

  removeProject: async (projectId: string) => {
    try {
      // Close PTY sessions for claude/ssh tabs belonging to this project
      const removedTabs = get().openTabs.filter(t => t.projectId === projectId);
      for (const tab of removedTabs) {
        if (tab.ptySessionId && (tab.type === 'claude' || tab.type === 'codex' || tab.type === 'ssh')) {
          useProcessStore.getState().setProcessState(tab.ptySessionId, { status: 'stopped', pid: null });
          invoke('close_pty', { id: tab.ptySessionId }).catch(() => {});
          invoke('unregister_process', { key: tab.ptySessionId }).catch(() => {});
          invoke('stop_resource_monitor', { commandId: tab.ptySessionId }).catch(() => {});
          cleanupSessionBuffer(tab.ptySessionId);
        }
      }

      const config = await invoke<AppConfig>('remove_project', { projectId });
      // Close tabs for removed project
      const openTabs = get().openTabs.filter(t => t.projectId !== projectId);
      const activeTabId = openTabs.find(t => t.id === get().activeTabId)
        ? get().activeTabId
        : openTabs[0]?.id ?? null;
      set({ config, openTabs, activeTabId });

      // Reinitialize git watcher without the removed project's folders
      const folderPaths: string[] = [];
      for (const project of config.projects) {
        for (const folder of project.folders) {
          folderPaths.push(folder.folderPath);
        }
      }
      invoke('watch_git_branches', { folderPaths }).catch(() => {});
    } catch (err) {
      console.error('Failed to remove project:', err);
    }
  },

  reorderProjects: async (projectIds: string[]) => {
    const config = get().config;
    if (!config) return;
    const projectMap = new Map(config.projects.map(p => [p.id, p]));
    const reordered = projectIds.map(id => projectMap.get(id)!).filter(Boolean);
    const newConfig = { ...config, projects: reordered };
    try {
      await invoke('save_full_config', { config: newConfig });
      set({ config: newConfig });
    } catch (err) {
      console.error('Failed to reorder projects:', err);
    }
  },

  updateSettings: async (settings: Settings) => {
    try {
      const config = await invoke<AppConfig>('update_settings', { settings });
      set({ config });
    } catch (err) {
      console.error('Failed to update settings:', err);
    }
  },

  addSSHConnection: async (conn: SSHConnection) => {
    const config = get().config;
    if (!config) return;
    const newConfig = { ...config, sshConnections: [...config.sshConnections, conn] };
    try {
      await invoke('save_full_config', { config: newConfig });
      set({ config: newConfig });
    } catch (err) {
      console.error('Failed to add SSH connection:', err);
    }
  },

  removeSSHConnection: async (connId: string) => {
    const config = get().config;
    if (!config) return;
    const newConfig = { ...config, sshConnections: config.sshConnections.filter(c => c.id !== connId) };
    // Close PTY sessions for SSH tabs belonging to this connection
    const removedTabs = get().openTabs.filter(t => t.sshConnectionId === connId);
    for (const tab of removedTabs) {
      if (tab.ptySessionId) {
        useProcessStore.getState().setProcessState(tab.ptySessionId, { status: 'stopped', pid: null });
        invoke('close_pty', { id: tab.ptySessionId }).catch(() => {});
        invoke('unregister_process', { key: tab.ptySessionId }).catch(() => {});
        invoke('stop_resource_monitor', { commandId: tab.ptySessionId }).catch(() => {});
        cleanupSessionBuffer(tab.ptySessionId);
      }
    }
    // Close tabs for this SSH connection
    const openTabs = get().openTabs.filter(t => t.sshConnectionId !== connId);
    const activeTabId = openTabs.find(t => t.id === get().activeTabId)
      ? get().activeTabId
      : openTabs[0]?.id ?? null;
    try {
      await invoke('save_full_config', { config: newConfig });
      set({ config: newConfig, openTabs, activeTabId });
    } catch (err) {
      console.error('Failed to remove SSH connection:', err);
    }
  },

  updateSSHConnection: async (conn: SSHConnection) => {
    const config = get().config;
    if (!config) return;
    const newConfig = {
      ...config,
      sshConnections: config.sshConnections.map(c => c.id === conn.id ? conn : c),
    };
    try {
      await invoke('save_full_config', { config: newConfig });
      set({ config: newConfig });
    } catch (err) {
      console.error('Failed to update SSH connection:', err);
    }
  },

  setActiveTab: (tabId) => {
    set({ activeTabId: tabId });
  },

  openTab: (tab: TabInfo) => {
    const { openTabs } = get();
    const existIdx = openTabs.findIndex(t => t.id === tab.id);
    if (existIdx >= 0) {
      // Merge new data into existing tab (e.g., ptySessionId on start)
      const updated = [...openTabs];
      updated[existIdx] = { ...updated[existIdx], ...tab };
      set({ openTabs: updated, activeTabId: tab.id });
      return;
    }
    set({
      openTabs: [...openTabs, tab],
      activeTabId: tab.id,
    });
  },

  openServerTab: (commandId: string, projectId: string) => {
    get().openTab({
      id: commandId,
      type: 'server',
      projectId,
      commandId,
      ptySessionId: commandId,
    });
  },

  closeTab: (tabId) => {
    const { openTabs, activeTabId } = get();
    const tab = openTabs.find(t => t.id === tabId);

    // Kill PTY session for claude/codex/ssh tabs
    if (tab?.ptySessionId && (tab.type === 'claude' || tab.type === 'codex' || tab.type === 'ssh')) {
      const sessionId = tab.ptySessionId;
      // Set stopped immediately so the running count updates
      useProcessStore.getState().setProcessState(sessionId, { status: 'stopped', pid: null });
      // Clean up backend resources (fire-and-forget)
      invoke('close_pty', { id: sessionId }).catch(() => {});
      invoke('unregister_process', { key: sessionId }).catch(() => {});
      invoke('stop_resource_monitor', { commandId: sessionId }).catch(() => {});
      cleanupSessionBuffer(sessionId);
    }

    const filtered = openTabs.filter(t => t.id !== tabId);
    let newActiveId = activeTabId;
    if (activeTabId === tabId) {
      const idx = openTabs.findIndex(t => t.id === tabId);
      newActiveId = filtered[Math.min(idx, filtered.length - 1)]?.id ?? null;
    }
    set({ openTabs: filtered, activeTabId: newActiveId });
  },

  reorderTabs: (tabs) => set({ openTabs: tabs }),

  toggleSidebar: () => set(s => ({ sidebarCollapsed: !s.sidebarCollapsed })),
  setSidebarCollapsed: (collapsed) => set({ sidebarCollapsed: collapsed }),

  loadSession: async () => {
    try {
      const session = await invoke<SessionState>('get_session');
      if (session.openTabs.length > 0) {
        const tabs: TabInfo[] = session.openTabs.map(st => ({
          id: st.id,
          type: st.type,
          projectId: st.projectId,
          commandId: st.commandId,
          ptySessionId: st.ptySessionId ?? (st.type === 'server' ? st.commandId : undefined),
          label: st.label,
          sshConnectionId: st.sshConnectionId,
        }));
        set({
          openTabs: tabs,
          activeTabId: session.activeTabId ?? tabs[0]?.id ?? null,
          sidebarCollapsed: session.sidebarCollapsed,
        });
      }
    } catch (err) {
      console.error('Failed to load session:', err);
    }
  },

  saveSession: async () => {
    const { openTabs, activeTabId, sidebarCollapsed } = get();
    const session: SessionState = {
      openTabs: openTabs.map(t => ({
        id: t.id,
        type: t.type,
        projectId: t.projectId,
        commandId: t.commandId,
        ptySessionId: t.ptySessionId,
        label: t.label,
        sshConnectionId: t.sshConnectionId,
      })),
      activeTabId,
      sidebarCollapsed,
    };
    try {
      await invoke('save_session', { session });
    } catch (err) {
      console.error('Failed to save session:', err);
    }
  },
}));
