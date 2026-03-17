import { listen, UnlistenFn } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { Terminal } from '@xterm/xterm';
import { useProcessStore } from '../stores/processStore';
import { useAppStore } from '../stores/appStore';

/** Get the pty session ID of the currently active tab */
function getActivePtySessionId(): string | null {
  const { activeTabId, openTabs } = useAppStore.getState();
  const activeTab = openTabs.find(t => t.id === activeTabId);
  return activeTab?.ptySessionId ?? null;
}

/** Get the configured notification sound (reads latest config each time) */
function getNotificationSound(): string | undefined {
  return useAppStore.getState().config?.settings.notificationSound;
}

// Persistent per-session state that survives component unmount/remount.
// Output buffering is handled entirely in Rust — JS stores nothing.
export interface SessionBuffer {
  exited: boolean;
  dataUnlisten: Promise<UnlistenFn>;
  exitUnlisten: Promise<UnlistenFn>;
  terminal: Terminal | null;     // current mounted terminal (null when unmounted)
  pendingQueue: Uint8Array[] | null;  // temporary queue while fetching Rust backlog on mount
  onExit?: () => void;
  trackActivity: boolean;        // whether to monitor for thinking/idle
  idleTimer: ReturnType<typeof setTimeout> | null;
  recentDataTimestamps: number[];
  suppressActivityUntil: number;
  pendingFrame: Uint8Array[] | null;  // chunks accumulated for rAF write batching
  writeRaf: number;                   // rAF handle for batched writes (0 = none pending)
}

export const sessionBuffers = new Map<string, SessionBuffer>();

export function ensureSessionBuffer(sessionId: string, onExit?: () => void, trackActivity = false): SessionBuffer {
  let buf = sessionBuffers.get(sessionId);
  if (buf) {
    buf.onExit = onExit;
    if (trackActivity) buf.trackActivity = true;
    return buf;
  }

  const entry: SessionBuffer = {
    exited: false,
    terminal: null,
    pendingQueue: null,
    onExit,
    trackActivity,
    idleTimer: null,
    recentDataTimestamps: [],
    suppressActivityUntil: 0,
    pendingFrame: null,
    writeRaf: 0,
    dataUnlisten: listen<string>(`pty-data-${sessionId}`, (event) => {
      const bytes = Uint8Array.from(atob(event.payload), c => c.charCodeAt(0));
      if (entry.terminal) {
        // Batch writes per animation frame. All PTY events arriving within a
        // frame are merged into a single terminal.write() so xterm processes
        // cursor-positioning escape sequences atomically — the cursor appears
        // only at its final position, not at intermediate TUI update positions.
        if (!entry.pendingFrame) entry.pendingFrame = [];
        entry.pendingFrame.push(bytes);
        if (!entry.writeRaf) {
          entry.writeRaf = requestAnimationFrame(() => {
            entry.writeRaf = 0;
            const chunks = entry.pendingFrame!;
            entry.pendingFrame = null;
            const total = chunks.reduce((s, c) => s + c.length, 0);
            const merged = new Uint8Array(total);
            let off = 0;
            for (const c of chunks) { merged.set(c, off); off += c.length; }
            entry.terminal?.write(merged);
          });
        }
      } else if (entry.pendingQueue) {
        // Terminal is mounting, waiting for Rust backlog — queue temporarily
        entry.pendingQueue.push(bytes);
      }
      // else: data is stored in Rust ring buffer, no JS storage needed

      // Activity detection based on data flow frequency
      if (entry.trackActivity) {
        const now = Date.now();
        if (now < entry.suppressActivityUntil) return;
        entry.recentDataTimestamps = entry.recentDataTimestamps.filter(t => now - t < 1000);
        entry.recentDataTimestamps.push(now);

        if (entry.recentDataTimestamps.length >= 3) {
          useProcessStore.getState().setTerminalActivity(sessionId, 'thinking', getActivePtySessionId(), getNotificationSound());
        }

        if (entry.idleTimer) clearTimeout(entry.idleTimer);
        entry.idleTimer = setTimeout(() => {
          entry.recentDataTimestamps = [];
          useProcessStore.getState().setTerminalActivity(sessionId, 'idle', getActivePtySessionId(), getNotificationSound());
        }, 3000);
      }
    }),
    exitUnlisten: listen<string>(`pty-exit-${sessionId}`, () => {
      entry.exited = true;
      if (entry.idleTimer) clearTimeout(entry.idleTimer);
      if (entry.trackActivity) {
        useProcessStore.getState().setTerminalActivity(sessionId, 'idle', getActivePtySessionId(), getNotificationSound());
      }
      if (entry.terminal) {
        entry.terminal.writeln('\r\n\x1b[90m--- Session ended ---\x1b[0m');
      }
      entry.onExit?.();
    }),
  };

  sessionBuffers.set(sessionId, entry);
  return entry;
}

export function cleanupSessionBuffer(sessionId: string) {
  const buf = sessionBuffers.get(sessionId);
  if (buf) {
    if (buf.writeRaf) cancelAnimationFrame(buf.writeRaf);
    buf.pendingFrame = null;
    if (buf.idleTimer) clearTimeout(buf.idleTimer);
    if (buf.trackActivity) {
      const store = useProcessStore.getState();
      const { [sessionId]: _t, ...restTitles } = store.terminalTitles;
      const { [sessionId]: _a, ...restActivity } = store.terminalActivity;
      useProcessStore.setState({ terminalTitles: restTitles, terminalActivity: restActivity });
    }
    buf.dataUnlisten.then(fn => fn());
    buf.exitUnlisten.then(fn => fn());
    sessionBuffers.delete(sessionId);
  }
}

/** Write text directly to the terminal display (not to the PTY process) */
export function writeToSessionTerminal(sessionId: string, text: string) {
  const buf = sessionBuffers.get(sessionId);
  if (buf?.terminal) {
    buf.terminal.write(text);
  }
}

/** Reset a session buffer for reuse (e.g., auto-restart) */
export function resetSessionForRestart(sessionId: string) {
  const buf = sessionBuffers.get(sessionId);
  if (buf) {
    buf.exited = false;
  }
  // Clear Rust-side ring buffer so stale output isn't replayed on remount
  invoke('drain_pty_buffer', { id: sessionId }).catch(() => {});
}
