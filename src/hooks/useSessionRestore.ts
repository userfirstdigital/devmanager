import { useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { useAppStore } from '../stores/appStore';
import { useProcessStore } from '../stores/processStore';

function resolveShellCommand(defaultTerminal: string): { command: string; args: string[] } {
  switch (defaultTerminal) {
    case 'powershell':
      return { command: 'powershell.exe', args: [] };
    case 'cmd':
      return { command: 'cmd.exe', args: [] };
    case 'bash':
    default:
      return { command: 'C:/Program Files/Git/bin/bash.exe', args: ['--login'] };
  }
}

export function useSessionRestore() {
  const saveTimeoutRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const openTabs = useAppStore(s => s.openTabs);
  const activeTabId = useAppStore(s => s.activeTabId);
  const sidebarCollapsed = useAppStore(s => s.sidebarCollapsed);
  const config = useAppStore(s => s.config);
  const loadConfig = useAppStore(s => s.loadConfig);
  const loadSession = useAppStore(s => s.loadSession);
  const saveSession = useAppStore(s => s.saveSession);

  // Initial load
  useEffect(() => {
    const init = async () => {
      await loadConfig();
      const cfg = useAppStore.getState().config;
      if (cfg?.settings.restoreSessionOnStart !== false) {
        await loadSession();

        // Restore claude tabs by relaunching fresh sessions
        const tabs = useAppStore.getState().openTabs;
        const defaultTerminal = cfg?.settings.defaultTerminal || 'bash';
        const shell = resolveShellCommand(defaultTerminal);

        for (const tab of tabs) {
          if (tab.type === 'claude' && tab.ptySessionId) {
            const project = cfg?.projects.find(p => p.id === tab.projectId);
            if (project) {
              try {
                const cwd = project.rootPath;
                const pid = await invoke<number>('create_pty_session', {
                  id: tab.ptySessionId,
                  cwd,
                  command: shell.command,
                  args: shell.args,
                  cols: 80,
                  rows: 24,
                });
                await invoke('register_process', {
                  key: tab.ptySessionId,
                  pid,
                  commandId: tab.ptySessionId,
                  projectId: tab.projectId,
                });
                await invoke('start_resource_monitor', {
                  commandId: tab.ptySessionId,
                  pid,
                });
                useProcessStore.getState().setProcessState(tab.ptySessionId, {
                  status: 'running',
                  pid,
                  startedAt: Date.now(),
                });
                // Launch Claude Code after shell init
                setTimeout(async () => {
                  try {
                    await invoke('write_pty', {
                      id: tab.ptySessionId,
                      data: 'npx @anthropic-ai/claude-code --dangerously-skip-permissions\r\n',
                    });
                  } catch { /* session may have closed */ }
                }, 500);
              } catch (err) {
                console.error('Failed to restore Claude tab:', err);
              }
            }
          }

          if (tab.type === 'ssh' && tab.ptySessionId && tab.sshConnectionId) {
            const conn = cfg?.sshConnections?.find(c => c.id === tab.sshConnectionId);
            if (conn) {
              try {
                const sshArgs = [
                  `${conn.username}@${conn.host}`,
                  '-p', String(conn.port),
                  '-o', 'StrictHostKeyChecking=no',
                ];
                const pid = await invoke<number>('create_pty_session', {
                  id: tab.ptySessionId,
                  cwd: '.',
                  command: 'ssh',
                  args: sshArgs,
                  cols: 80,
                  rows: 24,
                });
                await invoke('register_process', {
                  key: tab.ptySessionId,
                  pid,
                  commandId: tab.ptySessionId,
                  projectId: '',
                });
                await invoke('start_resource_monitor', {
                  commandId: tab.ptySessionId,
                  pid,
                });
                useProcessStore.getState().setProcessState(tab.ptySessionId, {
                  status: 'running',
                  pid,
                  startedAt: Date.now(),
                });

                // Auto-password
                if (conn.password) {
                  let passwordSent = false;
                  const unlisten = await listen<string>(`pty-data-${tab.ptySessionId}`, async (event) => {
                    if (passwordSent) return;
                    const bytes = Uint8Array.from(atob(event.payload), c => c.charCodeAt(0));
                    const text = new TextDecoder().decode(bytes);
                    if (/assword:/i.test(text)) {
                      passwordSent = true;
                      try {
                        await invoke('write_pty', { id: tab.ptySessionId, data: conn.password + '\n' });
                      } catch { /* session may have closed */ }
                      unlisten();
                    }
                  });
                  setTimeout(() => { if (!passwordSent) unlisten(); }, 30000);
                }
              } catch (err) {
                console.error('Failed to restore SSH tab:', err);
              }
            }
          }
        }
      }
    };
    init();
  }, []);

  // Auto-save session on tab changes (debounced)
  useEffect(() => {
    if (!config) return;
    if (saveTimeoutRef.current) clearTimeout(saveTimeoutRef.current);
    saveTimeoutRef.current = setTimeout(() => {
      saveSession();
    }, 1000);
    return () => {
      if (saveTimeoutRef.current) clearTimeout(saveTimeoutRef.current);
    };
  }, [openTabs, activeTabId, sidebarCollapsed]);
}
