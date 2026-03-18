import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { isPermissionGranted, requestPermission, sendNotification } from '@tauri-apps/plugin-notification';
import { useAppStore } from '../stores/appStore';
import { useProcessStore } from '../stores/processStore';
import type { RunCommand, ProjectFolder, EnvEntry } from '../types/config';
import { getAllCommands } from '../utils/projectHelpers';
import { ensureSessionBuffer, writeToSessionTerminal, resetSessionForRestart } from '../utils/terminalBuffers';
import { getPreferredPtySize } from '../utils/terminalSize';
import { buildServerLaunchCommand } from '../utils/runtimePlatform';

// Track auto-restart backoff per command
const restartBackoffs = new Map<string, { delay: number; lastCrash: number }>();

// Track pty-exit listeners so we can unlisten on re-start
const exitUnlisteners = new Map<string, () => void>();

export function useProcess() {
  const startProcess = async (folder: ProjectFolder, command: RunCommand, projectId: string) => {
    const processStore = useProcessStore.getState();
    const appStore = useAppStore.getState();
    const commandId = command.id;

    // Skip if already running or starting
    const existing = processStore.getProcess(commandId);
    if (existing?.status === 'running' || existing?.status === 'starting') {
      appStore.openServerTab(commandId, projectId);
      return;
    }

    // Init process state
    processStore.setProcessState(commandId, {
      status: 'starting',
      pid: null,
      exitCode: null,
      startedAt: null,
    });

    // Open the tab
    appStore.openServerTab(commandId, projectId);

    // Find project name for notifications
    const project = appStore.config?.projects.find(p => p.id === projectId);
    const projectName = project?.name ?? 'Unknown';
    const runtimeInfo = appStore.runtimeInfo;
    const settings = appStore.config?.settings ?? {
      theme: 'dark',
      logBufferSize: 10000,
      confirmOnClose: true,
      minimizeToTray: false,
      restoreSessionOnStart: true,
      defaultTerminal: 'bash' as const,
      macTerminalProfile: 'system' as const,
    };

    // Ensure session buffer exists before PTY starts so no data is lost
    ensureSessionBuffer(commandId);
    resetSessionForRestart(commandId);

    // Clean up previous exit listener if re-starting
    const prevUnlisten = exitUnlisteners.get(commandId);
    if (prevUnlisten) {
      prevUnlisten();
      exitUnlisteners.delete(commandId);
    }

    try {
      // Load .env file if configured on the folder
      let env: Record<string, string> = {};
      if (folder.envFilePath) {
        try {
          const envPath = `${folder.folderPath}/${folder.envFilePath}`;
          const entries = await invoke<EnvEntry[]>('read_env_file', { filePath: envPath });
          for (const entry of entries) {
            if (entry.type === 'variable' && entry.key && entry.value != null) {
              // Strip surrounding quotes from values
              let val = entry.value;
              if ((val.startsWith('"') && val.endsWith('"')) || (val.startsWith("'") && val.endsWith("'"))) {
                val = val.slice(1, -1);
              }
              env[entry.key] = val;
            }
          }
        } catch (err) {
          console.warn('Failed to load .env file:', err);
        }
      }
      // Command-level env vars override .env file
      if (command.env) {
        Object.assign(env, command.env);
      }

      // Build log file path if the project has logging enabled (default: true)
      let logFile: string | null = null;
      if (project && project.saveLogFiles !== false) {
        const folderName = folder.folderPath.replace(/\\/g, '/').split('/').filter(Boolean).pop() || 'unknown';
        const logFileName = `${folderName}-${command.label}`
          .toLowerCase()
          .replace(/[^a-z0-9]+/g, '-')
          .replace(/^-|-$/g, '') + '.log';
        logFile = `${project.rootPath}/${logFileName}`;
      }

      // Spawn PTY + register process + start monitor in one IPC call
      const launch = buildServerLaunchCommand(runtimeInfo, settings, command);
      const result = await invoke<{ pid: number; command_id: string }>('create_server_session', {
        id: commandId,
        cwd: folder.folderPath,
        command: launch.command,
        args: launch.args,
        env: Object.keys(env).length > 0 ? env : null,
        cols: getPreferredPtySize().cols,
        rows: getPreferredPtySize().rows,
        logFile,
        commandId,
        projectId,
      });

      const pid = result.pid;

      processStore.setProcessState(commandId, {
        status: 'running',
        pid,
        startedAt: Date.now(),
      });

      // Reset backoff after 60s of stability
      setTimeout(() => {
        const proc = useProcessStore.getState().getProcess(commandId);
        if (proc?.status === 'running') {
          restartBackoffs.delete(commandId);
        }
      }, 60000);

      // Listen for process exit
      const unlisten = await listen<string>(`pty-exit-${commandId}`, () => {
        unlisten();
        exitUnlisteners.delete(commandId);

        const proc = useProcessStore.getState().getProcess(commandId);
        if (proc?.status === 'stopping') {
          processStore.setProcessState(commandId, {
            status: 'stopped',
            pid: null,
            exitCode: 0,
          });
        } else {
          processStore.setProcessState(commandId, {
            status: 'crashed',
            pid: null,
            exitCode: 1,
          });

          // Send crash notification
          (async () => {
            try {
              let permissionGranted = await isPermissionGranted();
              if (!permissionGranted) {
                const permission = await requestPermission();
                permissionGranted = permission === 'granted';
              }
              if (permissionGranted) {
                sendNotification({
                  title: 'DevManager',
                  body: `${projectName} - ${command.label} crashed`,
                });
              }
            } catch { /* notification not critical */ }
          })();

          // Auto-restart with exponential backoff
          if (command.autoRestart) {
            const now = Date.now();
            const backoff = restartBackoffs.get(commandId);
            let delay = 1000;
            if (backoff && now - backoff.lastCrash < 60000) {
              delay = Math.min(backoff.delay * 2, 30000);
            }
            restartBackoffs.set(commandId, { delay, lastCrash: now });
            writeToSessionTerminal(commandId, `\r\n\x1b[33m--- Auto-restarting in ${delay / 1000}s... ---\x1b[0m\r\n`);
            setTimeout(() => startProcess(folder, command, projectId), delay);
          }
        }

        // Unregister process from Rust backend
        invoke('unregister_process', { key: commandId }).catch(console.error);
        invoke('stop_resource_monitor', { commandId }).catch(console.error);
      });

      exitUnlisteners.set(commandId, unlisten);

    } catch (err) {
      console.error('Failed to start process:', err);
      processStore.setProcessState(commandId, {
        status: 'crashed',
      });
      writeToSessionTerminal(commandId, `\r\n\x1b[31mFailed to start: ${err}\x1b[0m\r\n`);
    }
  };

  const stopProcess = async (commandId: string) => {
    const processStore = useProcessStore.getState();
    const proc = processStore.getProcess(commandId);
    if (!proc || proc.status === 'stopped' || proc.status === 'crashed') return;

    processStore.setProcessState(commandId, { status: 'stopping' });

    try {
      await invoke('close_pty', { id: commandId });
    } catch {
      // Fallback: kill process tree directly (only if we have a pid)
      if (proc.pid) {
        try {
          await invoke('kill_process_tree', { pid: proc.pid });
        } catch (err) {
          console.error('Failed to stop process:', err);
        }
      }
    }
  };

  const restartProcess = async (folder: ProjectFolder, command: RunCommand, projectId: string) => {
    const processStore = useProcessStore.getState();
    const proc = processStore.getProcess(command.id);

    if (proc?.pid) {
      processStore.setProcessState(command.id, { status: 'stopping' });
      try {
        await invoke('close_pty', { id: command.id });
      } catch {
        try { await invoke('kill_process_tree', { pid: proc.pid }); } catch {}
      }
      // Wait for process to fully terminate
      await new Promise(resolve => setTimeout(resolve, 1000));
    }

    writeToSessionTerminal(command.id, '\r\n\x1b[33m--- Restarting... ---\x1b[0m\r\n');
    await startProcess(folder, command, projectId);
  };

  const stopAllForProject = async (projectId: string) => {
    const appStore = useAppStore.getState();
    const processStore = useProcessStore.getState();
    const project = appStore.config?.projects.find(p => p.id === projectId);
    if (!project) return;

    const allCommands = getAllCommands(project);
    const promises = allCommands.map(cmd => {
      const proc = processStore.getProcess(cmd.id);
      if (proc?.status === 'running') {
        return stopProcess(cmd.id);
      }
      return Promise.resolve();
    });
    await Promise.all(promises);
  };

  const startAllForProject = async (projectId: string) => {
    const appStore = useAppStore.getState();
    const processStore = useProcessStore.getState();
    const project = appStore.config?.projects.find(p => p.id === projectId);
    if (!project) return;

    for (const folder of project.folders) {
      for (const cmd of folder.commands) {
        const proc = processStore.getProcess(cmd.id);
        if (proc?.status !== 'running') {
          await startProcess(folder, cmd, projectId);
          // Small delay between starts to avoid port races
          await new Promise(resolve => setTimeout(resolve, 500));
        }
      }
    }
  };

  const stopAll = async () => {
    const processStore = useProcessStore.getState();
    const promises = Object.entries(processStore.processes)
      .filter(([_, p]) => p.status === 'running')
      .map(([id]) => stopProcess(id));
    await Promise.all(promises);
  };

  const stopAllAndWait = async () => {
    const processStore = useProcessStore.getState();
    const managedIds = Object.entries(processStore.processes)
      .filter(([_, process]) => process.status === 'running' || process.status === 'stopping')
      .map(([id]) => id);

    await stopAll();

    if (managedIds.length === 0) return;

    await invoke('wait_for_managed_shutdown', { timeoutMs: 15000 });

    for (const id of managedIds) {
      processStore.setProcessState(id, {
        status: 'stopped',
        pid: null,
        exitCode: 0,
      });
    }
  };

  return {
    startProcess,
    stopProcess,
    restartProcess,
    stopAllForProject,
    startAllForProject,
    stopAll,
    stopAllAndWait,
  };
}
