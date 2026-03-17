import { useEffect, useRef, useCallback, useState } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import { invoke } from '@tauri-apps/api/core';
import { useProcessStore } from '../../stores/processStore';
import { useAppStore } from '../../stores/appStore';
import { ResourceMonitor } from '../servers/ResourceMonitor';
import { FontSizeSlider } from './FontSizeSlider';
import { ensureSessionBuffer } from '../../utils/terminalBuffers';
import { setPreferredPtySize, isAtBottom } from '../../utils/terminalSize';

interface InteractiveTerminalProps {
  sessionId: string;
  onExit?: () => void;
  showActivity?: boolean;
  hideCursor?: boolean;
  label?: string;
  isActive?: boolean;
}

const REPLAY_CHUNK_SIZE = 65536; // 64 KB per frame during backlog replay

export function InteractiveTerminal({ sessionId, onExit, showActivity = false, hideCursor = false, label, isActive = false }: InteractiveTerminalProps) {
  const termRef = useRef<HTMLDivElement>(null);
  const xtermRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);

  const defaultFontSize = useAppStore(s => s.config?.settings.terminalFontSize ?? 13);
  const [fontSize, setFontSize] = useState(defaultFontSize);
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

    terminal.open(termRef.current);
    fitAddon.fit();

    // Update shared preferred size so new PTY sessions start near real dims
    setPreferredPtySize(terminal.cols, terminal.rows);

    xtermRef.current = terminal;
    fitAddonRef.current = fitAddon;

    // Register with persistent session buffer
    const buf = ensureSessionBuffer(sessionId, onExit, showActivity);

    // --- Backlog replay (chunked) ---
    // Fetch snapshot from Rust ring buffer. While the async fetch is in-flight,
    // live events are captured in pendingQueue to preserve ordering.
    buf.pendingQueue = [];
    let replayAborted = false;

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
          // Final chunk — scroll after xterm processes it
          terminal.write(chunk, onDone);
        } else {
          terminal.write(chunk, () => {
            requestAnimationFrame(writeNext);
          });
        }
      };
      writeNext();
    };

    const finishMount = () => {
      if (buf.exited) {
        terminal.writeln('\r\n\x1b[90m--- Session ended ---\x1b[0m');
      }
      // Self-heal: verify PTY is alive and correct process state
      invoke<boolean>('check_pty_session', { id: sessionId }).then(alive => {
        if (alive) {
          const proc = useProcessStore.getState().getProcess(sessionId);
          if (!proc || proc.status !== 'running') {
            useProcessStore.getState().setProcessState(sessionId, {
              status: 'running',
              pid: proc?.pid ?? null,
              startedAt: proc?.startedAt ?? Date.now(),
            });
          }
        }
      }).catch(() => {});
    };

    invoke<string>('snapshot_pty_buffer', { id: sessionId }).then(data => {
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
        for (const c of parts) { merged.set(c, off); off += c.length; }
        replayChunked(merged, () => {
          if (isAtBottom(terminal)) terminal.scrollToBottom();
          finishMount();
        });
      } else {
        finishMount();
      }
    }).catch(() => {
      // Buffer fetch failed — go live immediately
      const queued = buf.pendingQueue ?? [];
      buf.pendingQueue = null;
      buf.terminal = terminal;

      if (queued.length > 0) {
        const total = queued.reduce((s, c) => s + c.length, 0);
        const merged = new Uint8Array(total);
        let off = 0;
        for (const c of queued) { merged.set(c, off); off += c.length; }
        replayChunked(merged, () => {
          if (isAtBottom(terminal)) terminal.scrollToBottom();
          finishMount();
        });
      } else {
        finishMount();
      }
    });

    // No scrollToBottom during live output — xterm's built-in auto-scroll
    // keeps the viewport pinned when viewportY === baseY at write time.
    // External scrollToBottom calls race with scroll events and cause
    // the viewport to snap back when the user tries to scroll up.

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

    // Ctrl+Enter → newline (DOM capture fires before xterm sees the event)
    const handleCtrlEnter = (e: KeyboardEvent) => {
      if (e.ctrlKey && e.key === 'Enter') {
        e.preventDefault();
        e.stopPropagation();
        invoke('write_pty', { id: sessionId, data: '\n' }).catch(() => {});
      }
    };
    termRef.current.addEventListener('keydown', handleCtrlEnter, true);

    // Debounced resize observer — prevents scroll jitter from rapid layout changes
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
            if (!termRef.current || termRef.current.clientWidth === 0) return;

            const dims = fitAddon.proposeDimensions();
            if (dims && dims.cols === lastCols && dims.rows === lastRows) {
              return;
            }

            // Snapshot scroll position before fit() reflows content
            const wasAtBottom = isAtBottom(terminal);

            fitAddon.fit();
            lastCols = terminal.cols;
            lastRows = terminal.rows;

            // Update shared preferred size
            setPreferredPtySize(terminal.cols, terminal.rows);

            handleResize(terminal.cols, terminal.rows);

            if (wasAtBottom) {
              terminal.scrollToBottom();
            }

            // Suppress activity detection for 2s after resize
            buf.suppressActivityUntil = Date.now() + 2000;
          } catch {
            // Ignore resize errors
          }
        });
      }, 100);
    });
    observer.observe(termRef.current);

    // Initial resize
    handleResize(terminal.cols, terminal.rows);

    return () => {
      replayAborted = true;
      observer.disconnect();
      if (resizeTimer) clearTimeout(resizeTimer);
      cancelAnimationFrame(resizeRaf);
      termRef.current?.removeEventListener('keydown', handleCtrlEnter, true);
      // Detach terminal from buffer — Rust ring buffer continues capturing output
      buf.terminal = null;
      buf.pendingQueue = null;
      terminal.dispose();
      xtermRef.current = null;
      fitAddonRef.current = null;
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
        {label && <span className="text-xs font-medium text-zinc-400 truncate">{label}</span>}
        <ResourceMonitor commandId={sessionId} />
        <div className="ml-auto">
          <FontSizeSlider value={fontSize} onChange={setFontSize} />
        </div>
      </div>
      <div ref={termRef} className="flex-1 bg-[#09090b] px-1" data-hide-cursor={hideCursor || undefined} />
    </div>
  );
}
