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
      fontSize,
      lineHeight: 1.3,
      scrollback: 10000,
      disableStdin: false,
      cursorStyle: 'bar',
      cursorBlink: true,
      convertEol: false,
      smoothScrollDuration: 0,
    });

    const fitAddon = new FitAddon();
    terminal.loadAddon(fitAddon);

    terminal.open(termRef.current);
    fitAddon.fit();

    xtermRef.current = terminal;
    fitAddonRef.current = fitAddon;

    // Register with persistent session buffer
    const buf = ensureSessionBuffer(sessionId, onExit, showActivity);

    // Fetch backlog from Rust ring buffer and replay to terminal.
    // Uses snapshot (non-destructive read) so buffer content survives webview refresh.
    // While the async fetch is in-flight, live events are captured in pendingQueue
    // to guarantee correct ordering: backlog first, then live data.
    buf.pendingQueue = [];
    invoke<string>('snapshot_pty_buffer', { id: sessionId }).then(data => {
      // Collect all data to write: snapshot + any queued live events
      const chunks: Uint8Array[] = [];
      if (data) {
        chunks.push(Uint8Array.from(atob(data), c => c.charCodeAt(0)));
      }
      if (buf.pendingQueue) {
        chunks.push(...buf.pendingQueue);
      }
      buf.pendingQueue = null;
      buf.terminal = terminal;

      if (chunks.length > 0) {
        // Concatenate and write once, scroll after xterm processes it
        const total = chunks.reduce((s, c) => s + c.length, 0);
        const merged = new Uint8Array(total);
        let off = 0;
        for (const c of chunks) { merged.set(c, off); off += c.length; }
        terminal.write(merged, () => {
          terminal.scrollToBottom();
        });
      }

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
    }).catch(() => {
      // Buffer fetch failed — go live immediately
      const chunks = buf.pendingQueue ?? [];
      buf.pendingQueue = null;
      buf.terminal = terminal;

      if (chunks.length > 0) {
        const total = chunks.reduce((s, c) => s + c.length, 0);
        const merged = new Uint8Array(total);
        let off = 0;
        for (const c of chunks) { merged.set(c, off); off += c.length; }
        terminal.write(merged, () => {
          terminal.scrollToBottom();
        });
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
    });

    // Auto-scroll: track whether the user is following output.
    // We use wheel events (not terminal.onScroll) to detect user intent —
    // onScroll fires on ALL viewport changes including internal xterm reflows,
    // which causes following to flicker on/off during rapid output bursts.
    let following = true;
    const handleWheel = (e: WheelEvent) => {
      if (e.deltaY < 0) {
        // User scrolled up — stop following
        following = false;
      } else if (e.deltaY > 0) {
        // User scrolled down — re-engage following unconditionally.
        // During rapid output baseY is a moving target, so checking
        // viewportY >= baseY is a race we can never win. Instead, just
        // re-enable following and let scrollToBottom snap to the true bottom.
        following = true;
      }
    };
    termRef.current.addEventListener('wheel', handleWheel, { passive: true });

    // scrollToBottom is called from xterm's write callback, which already
    // fires at most once per frame thanks to write batching in terminalBuffers.
    // No additional rAF throttle needed — adding one would create a one-frame
    // lag between content rendering and viewport scroll, causing cursor jumping.
    buf.onDataWritten = () => {
      if (following) {
        terminal.scrollToBottom();
      }
    };

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
    // (e.g., ResourceMonitor toggling between states)
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
            // Check proposed dimensions BEFORE fitting — fitAddon.fit()
            // itself causes a reflow that shifts the viewport, so we must
            // skip it entirely when cols/rows haven't changed.
            const dims = fitAddon.proposeDimensions();
            if (dims && dims.cols === lastCols && dims.rows === lastRows) {
              return;
            }

            // Remember if user was following output at the bottom
            const buf_active = terminal.buffer.active;
            const wasAtBottom = buf_active.viewportY >= buf_active.baseY;

            fitAddon.fit();
            lastCols = terminal.cols;
            lastRows = terminal.rows;

            handleResize(terminal.cols, terminal.rows);

            // Restore scroll position — keep user at bottom if they were there
            if (wasAtBottom) {
              terminal.scrollToBottom();
            }

            // Suppress activity detection for 2s after resize — the PTY redraws
            // the screen which generates data chunks that look like "thinking"
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
      observer.disconnect();
      if (resizeTimer) clearTimeout(resizeTimer);
      cancelAnimationFrame(resizeRaf);
      if (buf.writeRaf) cancelAnimationFrame(buf.writeRaf);
      buf.pendingFrame = null;
      termRef.current?.removeEventListener('wheel', handleWheel);
      termRef.current?.removeEventListener('keydown', handleCtrlEnter, true);
      // Detach terminal from buffer — Rust ring buffer continues capturing output
      buf.terminal = null;
      buf.pendingQueue = null;
      buf.onDataWritten = undefined;
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
      <div ref={termRef} className="flex-1 bg-[#09090b] px-1" />
    </div>
  );
}
