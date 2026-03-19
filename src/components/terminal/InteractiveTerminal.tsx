import { useEffect, useRef, useCallback, useState, type ReactNode } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import { invoke } from '@tauri-apps/api/core';
import { useProcessStore } from '../../stores/processStore';
import { useAppStore } from '../../stores/appStore';
import { ResourceMonitor } from '../servers/ResourceMonitor';
import { FontSizeSlider } from './FontSizeSlider';
import { ensureSessionBuffer } from '../../utils/terminalBuffers';
import {
  isAtBottom,
  restoreTerminalScrollState,
  setPreferredPtySize,
  snapshotTerminalScrollState,
} from '../../utils/terminalSize';
import { normalizeTerminalTitle } from '../../utils/tabTitles';

interface InteractiveTerminalProps {
  sessionId: string;
  onExit?: () => void;
  showActivity?: boolean;
  hideCursor?: boolean;
  label?: string;
  isActive?: boolean;
  headerActions?: ReactNode;
}

const REPLAY_CHUNK_SIZE = 65536; // 64 KB per frame during backlog replay

export function InteractiveTerminal({
  sessionId,
  onExit,
  showActivity = false,
  hideCursor = false,
  label,
  isActive = false,
  headerActions,
}: InteractiveTerminalProps) {
  const termRef = useRef<HTMLDivElement>(null);
  const xtermRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);

  const defaultFontSize = useAppStore(s => s.config?.settings.terminalFontSize ?? 13);
  const [fontSize, setFontSize] = useState(defaultFontSize);
  const setTerminalTitle = useProcessStore(s => s.setTerminalTitle);
  const clearTerminalTitle = useProcessStore(s => s.clearTerminalTitle);

  const handleResize = useCallback(async (cols: number, rows: number) => {
    try {
      await invoke('resize_pty', { id: sessionId, cols, rows });
    } catch {
      // Session may have closed
    }
  }, [sessionId]);

  useEffect(() => {
    let cancelled = false;
    let initRaf = 0;
    let cleanupTerminal: (() => void) | null = null;

    const mountTerminal = () => {
      const container = termRef.current;
      if (cancelled || !container) {
        return;
      }

      // In React StrictMode, the first dev-only mount is torn down immediately.
      // Defer xterm initialization until the container is attached and measurable
      // so the throwaway mount never calls `open()`.
      if (!container.isConnected || container.clientWidth === 0 || container.clientHeight === 0) {
        initRaf = requestAnimationFrame(mountTerminal);
        return;
      }

      const terminal = new Terminal({
        theme: {
          background: '#09090b',
          foreground: '#c8c8cd',
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
        fontSize,
        lineHeight: 1.3,
        scrollback: 10000,
        disableStdin: false,
        cursorStyle: 'bar',
        cursorBlink: !hideCursor,
        cursorInactiveStyle: hideCursor ? 'none' : 'outline',
        convertEol: false,
        smoothScrollDuration: 0,
      });

      const fitAddon = new FitAddon();
      terminal.loadAddon(fitAddon);

      terminal.open(container);
      fitAddon.fit();

      // Update shared preferred size so new PTY sessions start near real dims
      setPreferredPtySize(terminal.cols, terminal.rows);

      xtermRef.current = terminal;
      fitAddonRef.current = fitAddon;

      // Register with persistent session buffer
      const buf = ensureSessionBuffer(sessionId, onExit, showActivity);
      const viewportElement = container.querySelector<HTMLElement>('.xterm-viewport');
      const mountScrollState = {
        pinnedToBottom: buf.pinnedToBottom,
        viewportY: buf.viewportY,
      };

      buf.viewportElement = viewportElement;

      let replayAborted = false;
      let syncScrollLocked = true;
      let mountCompleted = false;

      const syncScrollState = () => {
        buf.pinnedToBottom = isAtBottom(terminal);
        buf.viewportY = terminal.buffer.active.viewportY;
      };

      const restoreScrollState = (state = mountScrollState) => {
        restoreTerminalScrollState(terminal, viewportElement, state);
      };

      const finishMount = () => {
        if (buf.exited) {
          terminal.writeln('\r\n\x1b[90m--- Session ended ---\x1b[0m');
        }
        // Self-heal: verify PTY is alive and correct process state
        invoke<boolean>('check_pty_session', { id: sessionId }).then(alive => {
          if (cancelled || !alive) {
            return;
          }
          const proc = useProcessStore.getState().getProcess(sessionId);
          if (!proc || proc.status !== 'running') {
            useProcessStore.getState().setProcessState(sessionId, {
              status: 'running',
              pid: proc?.pid ?? null,
              startedAt: proc?.startedAt ?? Date.now(),
            });
          }
        }).catch(() => {});
      };

      const completeMount = (state = mountScrollState) => {
        restoreScrollState(state);
        syncScrollLocked = false;
        syncScrollState();
        mountCompleted = true;
        finishMount();
      };

      const scrollDisposable = terminal.onScroll(() => {
        if (!syncScrollLocked) {
          syncScrollState();
        }
      });

      terminal.attachCustomWheelEventHandler((ev) => {
        if (ev.deltaY < 0 && terminal.buffer.active.baseY > 0) {
          // Detach from follow mode immediately so incoming output cannot snap
          // the viewport back down before xterm applies the wheel scroll.
          buf.pinnedToBottom = false;
          buf.viewportY = terminal.buffer.active.viewportY;
          return true;
        }

        if (ev.deltaY > 0 && terminal.buffer.active.baseY > 0 && isAtBottom(terminal)) {
          ev.preventDefault();
          ev.stopImmediatePropagation();
          restoreTerminalScrollState(terminal, viewportElement, {
            pinnedToBottom: true,
            viewportY: terminal.buffer.active.baseY,
          });
          syncScrollState();
          return false;
        }
        return true;
      });

      // --- Backlog replay (chunked) ---
      // Fetch snapshot from Rust ring buffer. While the async fetch is in-flight,
      // live events are captured in pendingQueue to preserve ordering.
      buf.pendingQueue = [];

      const replayChunked = (data: Uint8Array, onDone: () => void) => {
        let offset = 0;
        const writeNext = () => {
          if (replayAborted) return;
          if (offset >= data.length) {
            onDone();
            return;
          }
          const end = Math.min(offset + REPLAY_CHUNK_SIZE, data.length);
          const chunk = data.subarray(offset, end);
          offset = end;
          if (offset >= data.length) {
            // Final chunk - restore scroll state after xterm processes it.
            terminal.write(chunk, onDone);
          } else {
            terminal.write(chunk, () => {
              requestAnimationFrame(writeNext);
            });
          }
        };
        writeNext();
      };

      invoke<string>('snapshot_pty_buffer', { id: sessionId }).then(data => {
        if (cancelled) {
          return;
        }
        // Collect all data: snapshot + any queued live events
        const parts: Uint8Array[] = [];
        if (data) {
          parts.push(Uint8Array.from(atob(data), c => c.charCodeAt(0)));
        }
        if (buf.pendingQueue) {
          parts.push(...buf.pendingQueue);
        }
        buf.pendingQueue = null;
        buf.terminal = terminal;

        if (parts.length > 0) {
          const total = parts.reduce((s, c) => s + c.length, 0);
          const merged = new Uint8Array(total);
          let off = 0;
          for (const c of parts) {
            merged.set(c, off);
            off += c.length;
          }
          replayChunked(merged, () => {
            completeMount();
          });
        } else {
          completeMount();
        }
      }).catch(() => {
        if (cancelled) {
          return;
        }
        // Buffer fetch failed - go live immediately
        const queued = buf.pendingQueue ?? [];
        buf.pendingQueue = null;
        buf.terminal = terminal;

        if (queued.length > 0) {
          const total = queued.reduce((s, c) => s + c.length, 0);
          const merged = new Uint8Array(total);
          let off = 0;
          for (const c of queued) {
            merged.set(c, off);
            off += c.length;
          }
          replayChunked(merged, () => {
            completeMount();
          });
        } else {
          completeMount();
        }
      });

      // Send keystrokes to PTY
      terminal.onData(async (data) => {
        try {
          await invoke('write_pty', { id: sessionId, data });
        } catch {
          // Session may have closed
        }
      });

      // Track title changes for display purposes.
      terminal.onTitleChange((title) => {
        const normalizedTitle = normalizeTerminalTitle(title);
        if (normalizedTitle) {
          setTerminalTitle(sessionId, normalizedTitle);
        } else {
          clearTerminalTitle(sessionId);
        }
      });

      // Key handler: Ctrl+C (copy selection), Ctrl+V (paste)
      terminal.attachCustomKeyEventHandler((e) => {
        if (e.type === 'keydown' && e.ctrlKey) {
          if (!e.shiftKey && e.key === 'c') {
            const selection = terminal.getSelection();
            if (selection) {
              navigator.clipboard.writeText(selection);
              return false;
            }
          }
          if (!e.shiftKey && e.key === 'v') {
            return false;
          }
        }
        return true;
      });

      // Ctrl+Enter -> newline (DOM capture fires before xterm sees the event)
      const handleCtrlEnter = (e: KeyboardEvent) => {
        if (e.ctrlKey && e.key === 'Enter') {
          e.preventDefault();
          e.stopPropagation();
          invoke('write_pty', { id: sessionId, data: '\n' }).catch(() => {});
        }
      };
      container.addEventListener('keydown', handleCtrlEnter, true);

      // Debounced resize observer - prevents scroll jitter from rapid layout changes
      let resizeRaf = 0;
      let resizeTimer: ReturnType<typeof setTimeout> | null = null;
      let lastCols = terminal.cols;
      let lastRows = terminal.rows;
      const observer = new ResizeObserver(() => {
        if (resizeTimer) clearTimeout(resizeTimer);
        resizeTimer = setTimeout(() => {
          cancelAnimationFrame(resizeRaf);
          resizeRaf = requestAnimationFrame(() => {
            try {
              // Skip resize when container is hidden (display: none).
              if (container.clientWidth === 0 || container.clientHeight === 0) return;

              const dims = fitAddon.proposeDimensions();
              if (dims && dims.cols === lastCols && dims.rows === lastRows) {
                return;
              }

              const scrollState = snapshotTerminalScrollState(terminal, buf.pinnedToBottom);

              fitAddon.fit();
              lastCols = terminal.cols;
              lastRows = terminal.rows;

              // Update shared preferred size
              setPreferredPtySize(terminal.cols, terminal.rows);

              handleResize(terminal.cols, terminal.rows);
              restoreScrollState(scrollState);
              syncScrollState();

              // Suppress activity detection for 2s after resize
              buf.suppressActivityUntil = Date.now() + 2000;
            } catch {
              // Ignore resize errors
            }
          });
        }, 100);
      });
      observer.observe(container);

      // Initial resize
      handleResize(terminal.cols, terminal.rows);

      cleanupTerminal = () => {
        replayAborted = true;
        observer.disconnect();
        if (resizeTimer) clearTimeout(resizeTimer);
        cancelAnimationFrame(resizeRaf);
        scrollDisposable.dispose();
        container.removeEventListener('keydown', handleCtrlEnter, true);
        if (mountCompleted) {
          syncScrollState();
        } else {
          buf.pinnedToBottom = mountScrollState.pinnedToBottom;
          buf.viewportY = mountScrollState.viewportY;
        }
        // Detach terminal from buffer - Rust ring buffer continues capturing output
        buf.terminal = null;
        buf.viewportElement = null;
        buf.pendingQueue = null;
        terminal.dispose();
        xtermRef.current = null;
        fitAddonRef.current = null;
      };
    };

    initRaf = requestAnimationFrame(mountTerminal);

    return () => {
      cancelled = true;
      cancelAnimationFrame(initRaf);
      cleanupTerminal?.();
    };
  }, [sessionId, fontSize]);

  // Auto-focus terminal when this tab becomes active
  useEffect(() => {
    if (isActive && xtermRef.current) {
      xtermRef.current.focus();
    }
  }, [isActive]);

  return (
    <div className="h-full flex flex-col">
      <div className="flex items-center gap-3 px-3 h-8 shrink-0 bg-zinc-800/50 border-b border-zinc-700/50 overflow-hidden">
        <div className="min-w-0 flex-1">
          {label && <span className="block text-xs font-medium text-zinc-400 truncate">{label}</span>}
        </div>
        <div className="shrink-0">
          <ResourceMonitor commandId={sessionId} />
        </div>
        <div className="shrink-0">
          <FontSizeSlider value={fontSize} onChange={setFontSize} />
        </div>
        {headerActions && (
          <div className="shrink-0">
            {headerActions}
          </div>
        )}
      </div>
      <div ref={termRef} className="flex-1 bg-[#09090b] px-1" data-hide-cursor={hideCursor || undefined} />
    </div>
  );
}
