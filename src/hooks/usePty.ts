import { invoke } from '@tauri-apps/api/core';
import { useProcessStore } from '../stores/processStore';
import { cleanupSessionBuffer } from '../components/terminal/InteractiveTerminal';

export function usePty() {
  const createSession = async (
    id: string,
    cwd: string,
    command: string,
    args: string[] = [],
    env?: Record<string, string>,
    cols = 80,
    rows = 24,
  ): Promise<number> => {
    const pid = await invoke<number>('create_pty_session', {
      id,
      cwd,
      command,
      args,
      env,
      cols,
      rows,
    });

    // Register process for resource monitoring
    await invoke('register_process', {
      key: id,
      pid,
      commandId: id,
      projectId: id,
    });
    await invoke('start_resource_monitor', {
      commandId: id,
      pid,
    });

    const processStore = useProcessStore.getState();
    processStore.setProcessState(id, {
      status: 'running',
      pid,
      startedAt: Date.now(),
    });

    return pid;
  };

  const closeSession = async (id: string) => {
    cleanupSessionBuffer(id);
    try {
      await invoke('close_pty', { id });
    } catch (err) {
      console.error('Failed to close PTY session:', err);
    }
    try {
      await invoke('unregister_process', { key: id });
      await invoke('stop_resource_monitor', { commandId: id });
    } catch {
      // Best effort cleanup
    }
    const processStore = useProcessStore.getState();
    processStore.setProcessState(id, {
      status: 'stopped',
      pid: null,
    });
  };

  const writeSession = async (id: string, data: string) => {
    await invoke('write_pty', { id, data });
  };

  const resizeSession = async (id: string, cols: number, rows: number) => {
    await invoke('resize_pty', { id, cols, rows });
  };

  return {
    createSession,
    closeSession,
    writeSession,
    resizeSession,
  };
}
