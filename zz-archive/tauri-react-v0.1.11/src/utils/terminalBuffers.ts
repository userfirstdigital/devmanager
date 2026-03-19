import { type UnlistenFn } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { Terminal } from '@xterm/xterm';
import { useProcessStore } from '../stores/processStore';
import { useAppStore } from '../stores/appStore';
import { listenWithAutoCleanup } from './tauriListeners';
import {
  isAtBottom,
  restoreTerminalScrollState,
  snapshotTerminalScrollState,
} from './terminalSize';

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
  viewportElement: HTMLElement | null;
  pendingQueue: Uint8Array[] | null;  // temporary queue while fetching Rust backlog on mount
  onExit?: () => void;
  trackActivity: boolean;        // whether to monitor for thinking/idle
  idleTimer: ReturnType<typeof setTimeout> | null;
  recentDataTimestamps: number[];
  suppressActivityUntil: number;
  pendingFrame: Uint8Array[] | null;  // chunks accumulated for rAF write batching
  writeRaf: number;                   // rAF handle for batched writes (0 = none pending)
  pinnedToBottom: boolean;
  viewportY: number;
}

export const sessionBuffers = new Map<string, SessionBuffer>();

function syncSessionScrollState(entry: SessionBuffer, terminal = entry.terminal) {
  if (!terminal) return;
  entry.pinnedToBottom = isAtBottom(terminal);
  entry.viewportY = terminal.buffer.active.viewportY;
}

function writeToMountedTerminal(entry: SessionBuffer, data: string | Uint8Array) {
  const terminal = entry.terminal;
  if (!terminal) return;

  const shouldFollowOutput = entry.pinnedToBottom && isAtBottom(terminal);
  const scrollState = snapshotTerminalScrollState(terminal, shouldFollowOutput);
  terminal.write(data, () => {
    if (entry.terminal !== terminal) return;
    // Respect a user scroll-up that happened while xterm was processing the write.
    if (scrollState.pinnedToBottom && entry.pinnedToBottom) {
      restoreTerminalScrollState(terminal, entry.viewportElement, scrollState);
    }
    syncSessionScrollState(entry, terminal);
  });
}

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
    viewportElement: null,
    pendingQueue: null,
    onExit,
    trackActivity,
    idleTimer: null,
    recentDataTimestamps: [],
    suppressActivityUntil: 0,
    pendingFrame: null,
    writeRaf: 0,
    pinnedToBottom: true,
    viewportY: 0,
    dataUnlisten: listenWithAutoCleanup<string>(`pty-data-${sessionId}`, (event) => {
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
            writeToMountedTerminal(entry, merged);
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
    exitUnlisten: listenWithAutoCleanup<string>(`pty-exit-${sessionId}`, () => {
      entry.exited = true;
      if (entry.idleTimer) clearTimeout(entry.idleTimer);
      useProcessStore.getState().clearTerminalTitle(sessionId);
      if (entry.trackActivity) {
        useProcessStore.getState().setTerminalActivity(sessionId, 'idle', getActivePtySessionId(), getNotificationSound());
      }
      if (entry.terminal) {
        writeToMountedTerminal(entry, '\r\n\x1b[90m--- Session ended ---\x1b[0m\r\n');
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
    buf.viewportElement = null;
    if (buf.idleTimer) clearTimeout(buf.idleTimer);
    useProcessStore.getState().clearTerminalTitle(sessionId);
    if (buf.trackActivity) {
      const store = useProcessStore.getState();
      const { [sessionId]: _a, ...restActivity } = store.terminalActivity;
      useProcessStore.setState({ terminalActivity: restActivity });
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
    writeToMountedTerminal(buf, text);
  }
}

/** Reset a session buffer for reuse (e.g., auto-restart) */
export function resetSessionForRestart(sessionId: string) {
  const buf = sessionBuffers.get(sessionId);
  if (buf) {
    buf.exited = false;
  }
  useProcessStore.getState().clearTerminalTitle(sessionId);
  // Clear Rust-side ring buffer so stale output isn't replayed on remount
  invoke('drain_pty_buffer', { id: sessionId }).catch(() => {});
}
