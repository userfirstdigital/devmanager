import { useState, useRef, useEffect } from 'react';
import { Terminal, Plus, MoreVertical, Trash2 } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore, TabInfo } from '../../stores/appStore';
import { useProcessStore } from '../../stores/processStore';
import { usePty } from '../../hooks/usePty';
import { AddSSHDialog } from './AddSSHDialog';
import type { SSHConnection } from '../../types/config';
import { listenWithAutoCleanup } from '../../utils/tauriListeners';

export function SSHList() {
  const config = useAppStore(s => s.config);
  const openTabs = useAppStore(s => s.openTabs);
  const activeTabId = useAppStore(s => s.activeTabId);
  const { openTab, setActiveTab, removeSSHConnection } = useAppStore();
  const processes = useProcessStore(s => s.processes);
  const { createSession } = usePty();
  const [showAdd, setShowAdd] = useState(false);
  const [menuConnId, setMenuConnId] = useState<string | null>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  const connections = config?.sshConnections ?? [];

  useEffect(() => {
    const handleClick = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setMenuConnId(null);
      }
    };
    if (menuConnId) document.addEventListener('mousedown', handleClick);
    return () => document.removeEventListener('mousedown', handleClick);
  }, [menuConnId]);

  const handleConnect = async (conn: SSHConnection) => {
    // Check if already connected
    const existingTab = openTabs.find(t => t.type === 'ssh' && t.sshConnectionId === conn.id);
    if (existingTab) {
      setActiveTab(existingTab.id);
      return;
    }

    const sessionId = crypto.randomUUID();

    try {
      const sshArgs = [
        `${conn.username}@${conn.host}`,
        '-p', String(conn.port),
        '-o', 'StrictHostKeyChecking=no',
      ];

      await createSession(sessionId, '.', 'ssh', sshArgs);

      // Auto-password: watch for password prompt and enter it
      if (conn.password) {
        let passwordSent = false;
        const unlisten = await listenWithAutoCleanup<string>(`pty-data-${sessionId}`, async (event) => {
          if (passwordSent) return;
          const bytes = Uint8Array.from(atob(event.payload), c => c.charCodeAt(0));
          const text = new TextDecoder().decode(bytes);
          if (/assword:/i.test(text)) {
            passwordSent = true;
            try {
              await invoke('write_pty', { id: sessionId, data: conn.password + '\n' });
            } catch { /* session may have closed */ }
            unlisten();
          }
        });
        // Safety: stop listening after 30s regardless
        setTimeout(() => { if (!passwordSent) unlisten(); }, 30000);
      }

      const tab: TabInfo = {
        id: sessionId,
        type: 'ssh',
        projectId: '',
        ptySessionId: sessionId,
        sshConnectionId: conn.id,
        label: conn.label,
      };
      openTab(tab);
    } catch (err) {
      console.error('Failed to connect SSH:', err);
    }
  };

  if (connections.length === 0 && !showAdd) {
    return (
      <div className="px-3 py-2">
        <div className="flex items-center justify-between mb-1">
          <span className="text-[10px] font-semibold text-zinc-500 uppercase tracking-wider">SSH</span>
        </div>
        <button
          onClick={() => setShowAdd(true)}
          className="flex items-center gap-1.5 px-2 py-1 rounded hover:bg-zinc-700/30 text-xs text-cyan-400 hover:text-cyan-300 w-full"
        >
          <Plus size={11} />
          <span>Add SSH</span>
        </button>
        {showAdd && <AddSSHDialog onClose={() => setShowAdd(false)} />}
      </div>
    );
  }

  return (
    <div className="px-3 py-2">
      <div className="flex items-center justify-between mb-1">
        <span className="text-[10px] font-semibold text-zinc-500 uppercase tracking-wider">SSH</span>
      </div>
      <div className="space-y-0.5">
        {connections.map(conn => {
          const sshTab = openTabs.find(t => t.type === 'ssh' && t.sshConnectionId === conn.id);
          const proc = sshTab ? processes[sshTab.ptySessionId || ''] : null;
          const isConnected = proc?.status === 'running';

          return (
            <div
              key={conn.id}
              className={`group/ssh flex items-center gap-1.5 px-2 py-1 rounded cursor-pointer text-xs ${
                sshTab && activeTabId === sshTab.id ? 'bg-zinc-700/50' : 'hover:bg-zinc-700/30'
              }`}
              onClick={() => handleConnect(conn)}
            >
              <Terminal size={11} className="text-cyan-400 flex-shrink-0" />
              <span className="flex-1 text-zinc-300 truncate">{conn.label}</span>
              {isConnected && (
                <div className="w-1.5 h-1.5 rounded-full bg-emerald-400 flex-shrink-0" />
              )}
              <div className="relative flex-shrink-0" ref={menuConnId === conn.id ? menuRef : undefined}>
                <button
                  onClick={(e) => { e.stopPropagation(); setMenuConnId(menuConnId === conn.id ? null : conn.id); }}
                  className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-zinc-300 opacity-0 group-hover/ssh:opacity-100"
                >
                  <MoreVertical size={12} />
                </button>
                {menuConnId === conn.id && (
                  <div className="absolute right-0 top-full mt-1 z-50 w-36 bg-zinc-800 border border-zinc-600 rounded-md shadow-lg py-1">
                    <button
                      onClick={(e) => { e.stopPropagation(); removeSSHConnection(conn.id); setMenuConnId(null); }}
                      className="w-full text-left px-3 py-1.5 text-xs text-red-400 hover:bg-zinc-700 flex items-center gap-2"
                    >
                      <Trash2 size={12} /> Remove
                    </button>
                  </div>
                )}
              </div>
            </div>
          );
        })}
        <button
          onClick={() => setShowAdd(true)}
          className="flex items-center gap-1.5 px-2 py-1 rounded hover:bg-zinc-700/30 text-xs text-cyan-400 hover:text-cyan-300 w-full"
        >
          <Plus size={11} />
          <span>Add SSH</span>
        </button>
      </div>
      {showAdd && <AddSSHDialog onClose={() => setShowAdd(false)} />}
    </div>
  );
}
