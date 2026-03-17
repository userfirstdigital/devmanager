import { useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { useAppStore } from '../stores/appStore';
import { useProcessStore } from '../stores/processStore';
import { ensureSessionBuffer } from '../utils/terminalBuffers';
import { getPreferredPtySize } from '../utils/terminalSize';

const DEFAULT_CLAUDE_CMD = 'npx -y @anthropic-ai/claude-code@latest --dangerously-skip-permissions';
const DEFAULT_CODEX_CMD = 'npx -y @openai/codex@latest --dangerously-bypass-approvals-and-sandbox';

interface RestoreRequest {
  id: string;
  cwd: string;
  command: string;
  args: string[];
  env?: Record<string, string>;
  cols?: number;
  rows?: number;
  projectId: string;
  checkAlive: boolean;
}

interface RestoreResult {
  id: string;
  pid: number | null;
  alive: boolean;
  error: string | null;
}

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

        const tabs = useAppStore.getState().openTabs;
        const defaultTerminal = cfg?.settings.defaultTerminal || 'bash';
        const shell = resolveShellCommand(defaultTerminal);

        // Build batch restore requests for claude, codex, and ssh tabs
        const restoreRequests: RestoreRequest[] = [];
        const tabIndexMap: { tabIndex: number; reqIndex: number; type: 'claude' | 'codex' | 'ssh'; sshPassword?: string }[] = [];

        for (let i = 0; i < tabs.length; i++) {
          const tab = tabs[i];

          if ((tab.type === 'claude' || tab.type === 'codex') && tab.ptySessionId) {
            const project = cfg?.projects.find(p => p.id === tab.projectId);
            if (project) {
              tabIndexMap.push({ tabIndex: i, reqIndex: restoreRequests.length, type: tab.type });
              restoreRequests.push({
                id: tab.ptySessionId,
                cwd: project.rootPath,
                command: shell.command,
                args: shell.args,
                cols: getPreferredPtySize().cols,
                rows: getPreferredPtySize().rows,
                projectId: tab.projectId,
                checkAlive: true,
              });
            }
          }

          if (tab.type === 'ssh' && tab.ptySessionId && tab.sshConnectionId) {
            const conn = cfg?.sshConnections?.find(c => c.id === tab.sshConnectionId);
            if (conn) {
              const sshArgs = [
                `${conn.username}@${conn.host}`,
                '-p', String(conn.port),
                '-o', 'StrictHostKeyChecking=no',
              ];
              tabIndexMap.push({
                tabIndex: i,
                reqIndex: restoreRequests.length,
                type: 'ssh',
                sshPassword: conn.password,
              });
              restoreRequests.push({
                id: tab.ptySessionId,
                cwd: '.',
                command: 'ssh',
                args: sshArgs,
                cols: getPreferredPtySize().cols,
                rows: getPreferredPtySize().rows,
                projectId: '',
                checkAlive: true,
              });
            }
          }
        }

        // Single batch IPC call to restore all sessions
        if (restoreRequests.length > 0) {
          try {
            const results = await invoke<RestoreResult[]>('restore_sessions', { requests: restoreRequests });

            for (const entry of tabIndexMap) {
              const result = results[entry.reqIndex];
              const tab = tabs[entry.tabIndex];
              if (!result || !tab.ptySessionId) continue;

              if (result.alive) {
                // Session survived from previous run
                const isAI = entry.type === 'claude' || entry.type === 'codex';
                ensureSessionBuffer(tab.ptySessionId, undefined, isAI);
                useProcessStore.getState().setProcessState(tab.ptySessionId, {
                  status: 'running',
                  pid: null,
                  startedAt: Date.now(),
                });
                console.log(`[SessionRestore] Reconnected to existing session: ${tab.ptySessionId}`);
              } else if (result.pid != null) {
                // Fresh session created
                useProcessStore.getState().setProcessState(tab.ptySessionId, {
                  status: 'running',
                  pid: result.pid,
                  startedAt: Date.now(),
                });

                if (entry.type === 'claude' || entry.type === 'codex') {
                  // Launch AI command after shell init
                  const aiCmd = entry.type === 'claude'
                    ? (cfg?.settings.claudeCommand || DEFAULT_CLAUDE_CMD)
                    : (cfg?.settings.codexCommand || DEFAULT_CODEX_CMD);
                  const sessionId = tab.ptySessionId;
                  setTimeout(async () => {
                    try {
                      await invoke('write_pty', {
                        id: sessionId,
                        data: aiCmd + '\r\n',
                      });
                    } catch { /* session may have closed */ }
                  }, 500);
                }

                if (entry.type === 'ssh' && entry.sshPassword) {
                  // Auto-password for SSH
                  const sessionId = tab.ptySessionId;
                  const password = entry.sshPassword;
                  let passwordSent = false;
                  const unlisten = await listen<string>(`pty-data-${sessionId}`, async (event) => {
                    if (passwordSent) return;
                    const bytes = Uint8Array.from(atob(event.payload), c => c.charCodeAt(0));
                    const text = new TextDecoder().decode(bytes);
                    if (/assword:/i.test(text)) {
                      passwordSent = true;
                      try {
                        await invoke('write_pty', { id: sessionId, data: password + '\n' });
                      } catch { /* session may have closed */ }
                      unlisten();
                    }
                  });
                  setTimeout(() => { if (!passwordSent) unlisten(); }, 30000);
                }
              } else if (result.error) {
                console.error(`Failed to restore session ${result.id}:`, result.error);
              }
            }
          } catch (err) {
            console.error('Failed to batch restore sessions:', err);
          }
        }

        // Reconnect server tabs to alive backend sessions
        const serverTabs = tabs.filter(t => t.type === 'server' && t.ptySessionId);
        for (const tab of serverTabs) {
          try {
            const alive = await invoke<boolean>('check_pty_session', { id: tab.ptySessionId });
            if (alive) {
              ensureSessionBuffer(tab.ptySessionId!);
              useProcessStore.getState().setProcessState(tab.ptySessionId!, {
                status: 'running',
                pid: null,
                startedAt: Date.now(),
              });
            }
          } catch {}
        }
      }

      // Start git branch file watcher after config loads
      const finalCfg = useAppStore.getState().config;
      if (finalCfg) {
        const folderPaths: string[] = [];
        for (const project of finalCfg.projects) {
          for (const folder of project.folders) {
            folderPaths.push(folder.folderPath);
          }
        }
        if (folderPaths.length > 0) {
          invoke('watch_git_branches', { folderPaths }).catch(err => {
            console.warn('Failed to start git branch watcher:', err);
          });
        }
      }
    };
    init();
  }, []);

  // Visibility change reconnection — reconcile process state when window becomes visible after >30s hidden
  useEffect(() => {
    let lastHidden = 0;

    const handleVisibility = async () => {
      if (document.visibilityState === 'hidden') {
        lastHidden = Date.now();
        return;
      }
      if (lastHidden > 0 && Date.now() - lastHidden > 30_000) {
        const tabs = useAppStore.getState().openTabs;
        for (const tab of tabs) {
          const sessionId = tab.ptySessionId;
          if (!sessionId) continue;
          try {
            const alive = await invoke<boolean>('check_pty_session', { id: sessionId });
            const current = useProcessStore.getState().getProcess(sessionId);
            if (alive && current?.status !== 'running') {
              useProcessStore.getState().setProcessState(sessionId, {
                status: 'running',
                pid: current?.pid ?? null,
                startedAt: current?.startedAt ?? Date.now(),
              });
            } else if (!alive && current?.status === 'running') {
              useProcessStore.getState().setProcessState(sessionId, {
                status: 'stopped',
                pid: null,
              });
            }
          } catch {}
        }
      }
    };

    document.addEventListener('visibilitychange', handleVisibility);
    return () => document.removeEventListener('visibilitychange', handleVisibility);
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
