import { invoke } from '@tauri-apps/api/core';
import { useProcessStore } from '../stores/processStore';
import { cleanupSessionBuffer } from '../utils/terminalBuffers';
import { getPreferredPtySize } from '../utils/terminalSize';

export function usePty() {
  const createSession = async (
    id: string,
    cwd: string,
    command: string,
    args: string[] = [],
    env?: Record<string, string>,
    cols?: number,
    rows?: number,
  ): Promise<number> => {
    const preferred = getPreferredPtySize();
    cols = cols ?? preferred.cols;
    rows = rows ?? preferred.rows;
    // Single IPC call: create PTY + register process + start resource monitor
    const result = await invoke<{ pid: number; command_id: string }>('create_server_session', {
      id,
      cwd,
      command,
      args,
      env,
      cols,
      rows,
      commandId: id,
      projectId: id,
    });

    const processStore = useProcessStore.getState();
    processStore.setProcessState(id, {
      status: 'running',
      pid: result.pid,
      startedAt: Date.now(),
    });

    return result.pid;
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
