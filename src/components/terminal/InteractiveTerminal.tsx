import { useEffect, useRef, useCallback } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import { listen, UnlistenFn } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { useProcessStore } from '../../stores/processStore';
import { useAppStore } from '../../stores/appStore';
import { ResourceMonitor } from '../servers/ResourceMonitor';

/** Get the pty session ID of the currently active tab */
function getActivePtySessionId(): string | null {
  const { activeTabId, openTabs } = useAppStore.getState();
  const activeTab = openTabs.find(t => t.id === activeTabId);
  return activeTab?.ptySessionId ?? null;
}

// Persistent per-session state that survives component unmount/remount
interface SessionBuffer {
  chunks: Uint8Array[];       // buffered PTY output
  exited: boolean;
  dataUnlisten: Promise<UnlistenFn>;
  exitUnlisten: Promise<UnlistenFn>;
  terminal: Terminal | null;  // current mounted terminal (null when unmounted)
  onExit?: () => void;
  trackActivity: boolean;     // whether to monitor for thinking/idle
  idleTimer: ReturnType<typeof setTimeout> | null;
  recentDataTimestamps: number[];  // timestamps of recent data chunks for frequency detection
  suppressActivityUntil: number;   // ignore activity detection until this timestamp (after resize)
}

const sessionBuffers = new Map<string, SessionBuffer>();

function ensureSessionBuffer(sessionId: string, onExit?: () => void, trackActivity = false): SessionBuffer {
  let buf = sessionBuffers.get(sessionId);
  if (buf) {
    buf.onExit = onExit;
    if (trackActivity) buf.trackActivity = true;
    return buf;
  }

  const chunks: Uint8Array[] = [];

  const entry: SessionBuffer = {
    chunks,
    exited: false,
    terminal: null,
    onExit,
    trackActivity,
    idleTimer: null,
    recentDataTimestamps: [],
    suppressActivityUntil: 0,
    dataUnlisten: listen<string>(`pty-data-${sessionId}`, (event) => {
      const bytes = Uint8Array.from(atob(event.payload), c => c.charCodeAt(0));
      if (entry.terminal) {
        // Terminal is mounted — write directly
        entry.terminal.write(bytes);
      } else {
        // Terminal is unmounted — buffer
        entry.chunks.push(bytes);
      }

      // Activity detection based on data flow frequency
      // When Claude is thinking/working, data arrives in rapid bursts (spinner, streaming)
      // When idle/waiting for input, no data flows
      if (entry.trackActivity) {
        const now = Date.now();
        // Skip detection during suppress window (e.g., after a terminal resize)
        if (now < entry.suppressActivityUntil) return;
        // Keep only timestamps from the last 1 second
        entry.recentDataTimestamps = entry.recentDataTimestamps.filter(t => now - t < 1000);
        entry.recentDataTimestamps.push(now);

        // 3+ data chunks per second indicates active processing (spinner animation, streaming output)
        // Single chunks are likely just user keystroke echoes
        if (entry.recentDataTimestamps.length >= 3) {
          useProcessStore.getState().setTerminalActivity(sessionId, 'thinking', getActivePtySessionId());
        }

        // Reset idle timer — after 3s of no data, transition to idle
        if (entry.idleTimer) clearTimeout(entry.idleTimer);
        entry.idleTimer = setTimeout(() => {
          entry.recentDataTimestamps = [];
          useProcessStore.getState().setTerminalActivity(sessionId, 'idle', getActivePtySessionId());
        }, 3000);
      }
    }),
    exitUnlisten: listen<string>(`pty-exit-${sessionId}`, () => {
      entry.exited = true;
      if (entry.idleTimer) clearTimeout(entry.idleTimer);
      if (entry.trackActivity) {
        useProcessStore.getState().setTerminalActivity(sessionId, 'idle', getActivePtySessionId());
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
    if (buf.idleTimer) clearTimeout(buf.idleTimer);
    if (buf.trackActivity) {
      const store = useProcessStore.getState();
      // Clean up activity tracking state
      const { [sessionId]: _t, ...restTitles } = store.terminalTitles;
      const { [sessionId]: _a, ...restActivity } = store.terminalActivity;
      useProcessStore.setState({ terminalTitles: restTitles, terminalActivity: restActivity });
    }
    buf.dataUnlisten.then(fn => fn());
    buf.exitUnlisten.then(fn => fn());
    sessionBuffers.delete(sessionId);
  }
}

/** Pre-create a session buffer so PTY data is captured before the component mounts */
export { ensureSessionBuffer };

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
    buf.chunks.length = 0;
  }
}

interface InteractiveTerminalProps {
  sessionId: string;
  onExit?: () => void;
  showActivity?: boolean;
  label?: string;
  isActive?: boolean;
}

export function InteractiveTerminal({ sessionId, onExit, showActivity = false, label, isActive = false }: InteractiveTerminalProps) {
  const termRef = useRef<HTMLDivElement>(null);
  const xtermRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);

  const setTerminalTitle = useProcessStore(s => s.setTerminalTitle);

  const handleResize = useCallback(async (cols: number, rows: number) => {
    try {
      await invoke('resize_pty', { id: sessionId, cols, rows });
    } catch {
      // Session may have closed
    }
  }, [sessionId]);

  useEffect(() => {
    if (!termRef.current) return;

    const terminal = new Terminal({
      theme: {
        background: '#09090b',
        foreground: '#e4e4e7',
        cursor: '#e4e4e7',
        selectionBackground: '#3f3f46',
        black: '#18181b',
        red: '#ef4444',
        green: '#22c55e',
        yellow: '#eab308',
        blue: '#3b82f6',
        magenta: '#a855f7',
        cyan: '#06b6d4',
        white: '#e4e4e7',
        brightBlack: '#52525b',
        brightRed: '#f87171',
        brightGreen: '#4ade80',
        brightYellow: '#facc15',
        brightBlue: '#60a5fa',
        brightMagenta: '#c084fc',
        brightCyan: '#22d3ee',
        brightWhite: '#fafafa',
      },
      fontFamily: '"Cascadia Code", "Fira Code", "JetBrains Mono", Consolas, monospace',
      fontSize: 13,
      lineHeight: 1.3,
      scrollback: 10000,
      disableStdin: false,
      cursorStyle: 'bar',
      cursorBlink: true,
      convertEol: false,
    });

    const fitAddon = new FitAddon();
    terminal.loadAddon(fitAddon);

    terminal.open(termRef.current);
    fitAddon.fit();

    xtermRef.current = terminal;
    fitAddonRef.current = fitAddon;

    // Register with persistent session buffer
    const buf = ensureSessionBuffer(sessionId, onExit, showActivity);
    buf.terminal = terminal;

    // Replay any buffered data from while we were unmounted
    if (buf.chunks.length > 0) {
      for (const chunk of buf.chunks) {
        terminal.write(chunk);
      }
      buf.chunks.length = 0;
    }
    if (buf.exited) {
      terminal.writeln('\r\n\x1b[90m--- Session ended ---\x1b[0m');
    }

    // Send keystrokes to PTY
    terminal.onData(async (data) => {
      try {
        await invoke('write_pty', { id: sessionId, data });
      } catch {
        // Session may have closed
      }
    });

    // Track title changes for display purposes
    if (showActivity) {
      terminal.onTitleChange((title) => {
        setTerminalTitle(sessionId, title);
      });
    }

    // Ctrl+C copies when there's a selection, otherwise sends to PTY
    // Ctrl+V pastes from clipboard into PTY
    terminal.attachCustomKeyEventHandler((e) => {
      if (e.type === 'keydown' && e.ctrlKey && !e.shiftKey) {
        if (e.key === 'c') {
          const selection = terminal.getSelection();
          if (selection) {
            navigator.clipboard.writeText(selection);
            return false;
          }
        }
        if (e.key === 'v') {
          navigator.clipboard.readText().then(text => {
            if (text) {
              invoke('write_pty', { id: sessionId, data: text }).catch(() => {});
            }
          }).catch(() => {});
          return false;
        }
      }
      return true;
    });

    // Resize observer
    const observer = new ResizeObserver(() => {
      try {
        fitAddon.fit();
        handleResize(terminal.cols, terminal.rows);
        // Suppress activity detection for 2s after resize — the PTY redraws
        // the screen which generates data chunks that look like "thinking"
        buf.suppressActivityUntil = Date.now() + 2000;
      } catch {
        // Ignore resize errors
      }
    });
    observer.observe(termRef.current);

    // Initial resize
    handleResize(terminal.cols, terminal.rows);

    return () => {
      observer.disconnect();
      // Detach terminal from buffer but keep the buffer + listeners alive
      buf.terminal = null;
      terminal.dispose();
      xtermRef.current = null;
      fitAddonRef.current = null;
    };
  }, [sessionId]);

  // Auto-focus terminal when this tab becomes active
  useEffect(() => {
    if (isActive && xtermRef.current) {
      xtermRef.current.focus();
    }
  }, [isActive]);

  return (
    <div className="h-full flex flex-col">
      <div className="flex items-center gap-3 px-3 py-1.5 bg-zinc-800/50 border-b border-zinc-700/50">
        {label && <span className="text-xs font-medium text-zinc-400 truncate">{label}</span>}
        <ResourceMonitor commandId={sessionId} />
      </div>
      <div ref={termRef} className="flex-1 bg-[#09090b] px-1" />
    </div>
  );
}
