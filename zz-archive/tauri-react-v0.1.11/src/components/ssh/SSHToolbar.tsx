import { Unplug, RefreshCw } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../../stores/appStore';
import { useProcessStore } from '../../stores/processStore';
import { usePty } from '../../hooks/usePty';
import { listenWithAutoCleanup } from '../../utils/tauriListeners';

interface SSHToolbarProps {
  sshConnectionId?: string;
  ptySessionId?: string;
}

export function SSHToolbar({ sshConnectionId, ptySessionId }: SSHToolbarProps) {
  const config = useAppStore(s => s.config);
  const proc = useProcessStore(s => s.processes[ptySessionId || '']);
  const { closeSession, createSession } = usePty();

  const conn = config?.sshConnections?.find(c => c.id === sshConnectionId);
  const isConnected = proc?.status === 'running';

  const handleDisconnect = async () => {
    if (ptySessionId) {
      await closeSession(ptySessionId);
    }
  };

  const handleReconnect = async () => {
    if (!conn || !ptySessionId) return;

    // Close existing session
    await closeSession(ptySessionId);

    // Wait briefly for cleanup
    await new Promise(r => setTimeout(r, 500));

    // Reconnect with same session ID
    const sshArgs = [
      `${conn.username}@${conn.host}`,
      '-p', String(conn.port),
      '-o', 'StrictHostKeyChecking=no',
    ];

    try {
      await createSession(ptySessionId, '.', 'ssh', sshArgs);

      // Auto-password
      if (conn.password) {
        let passwordSent = false;
        const unlisten = await listenWithAutoCleanup<string>(`pty-data-${ptySessionId}`, async (event) => {
          if (passwordSent) return;
          const bytes = Uint8Array.from(atob(event.payload), c => c.charCodeAt(0));
          const text = new TextDecoder().decode(bytes);
          if (/assword:/i.test(text)) {
            passwordSent = true;
            try {
              await invoke('write_pty', { id: ptySessionId, data: conn.password + '\n' });
            } catch { /* session may have closed */ }
            unlisten();
          }
        });
        setTimeout(() => { if (!passwordSent) unlisten(); }, 30000);
      }
    } catch (err) {
      console.error('Failed to reconnect SSH:', err);
    }
  };

  return (
    <div className="flex items-center gap-2 px-3 py-1.5 bg-zinc-800 border-t border-zinc-700">
      <div className="flex items-center gap-1.5 flex-1">
        <div className={`w-1.5 h-1.5 rounded-full ${isConnected ? 'bg-emerald-400' : 'bg-zinc-600'}`} />
        <span className="text-xs text-zinc-400">
          {conn ? `${conn.username}@${conn.host}:${conn.port}` : 'SSH'}
        </span>
        <span className={`text-[10px] ${isConnected ? 'text-emerald-400' : 'text-zinc-600'}`}>
          {isConnected ? 'connected' : 'disconnected'}
        </span>
      </div>
      <button
        onClick={handleDisconnect}
        disabled={!isConnected}
        className="p-1 rounded hover:bg-zinc-700 text-zinc-400 hover:text-red-400 disabled:opacity-30"
        title="Disconnect"
      >
        <Unplug size={14} />
      </button>
      <button
        onClick={handleReconnect}
        disabled={isConnected}
        className="p-1 rounded hover:bg-zinc-700 text-zinc-400 hover:text-emerald-400 disabled:opacity-30"
        title="Reconnect"
      >
        <RefreshCw size={14} />
      </button>
    </div>
  );
}
