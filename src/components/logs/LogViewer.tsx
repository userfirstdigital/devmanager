import { useEffect, useRef } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { SearchAddon } from '@xterm/addon-search';
import '@xterm/xterm/css/xterm.css';
import { useProcessStore } from '../../stores/processStore';
import { ServerControls } from '../servers/ServerControls';
import { ResourceMonitor } from '../servers/ResourceMonitor';

export function LogViewer({ commandId }: { commandId: string }) {
  const termRef = useRef<HTMLDivElement>(null);
  const xtermRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);
  const searchAddonRef = useRef<SearchAddon | null>(null);
  const lastLogIndexRef = useRef<number>(0);
  const autoScrollRef = useRef(true);

  const proc = useProcessStore(s => s.processes[commandId]);
  const logs = proc?.logs ?? [];
  const resetUnseenErrors = useProcessStore(s => s.resetUnseenErrors);

  // Reset unseen errors when this tab becomes active
  useEffect(() => {
    resetUnseenErrors(commandId);
  }, [commandId]);

  // Initialize terminal
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
      disableStdin: true,
      cursorStyle: 'bar',
      cursorBlink: false,
      convertEol: true,
    });

    const fitAddon = new FitAddon();
    const searchAddon = new SearchAddon();
    terminal.loadAddon(fitAddon);
    terminal.loadAddon(searchAddon);

    terminal.open(termRef.current);
    fitAddon.fit();

    // Ctrl+C / Ctrl+Shift+C to copy selected text
    terminal.attachCustomKeyEventHandler((e) => {
      if (e.type === 'keydown' && e.key === 'c' && e.ctrlKey) {
        const selection = terminal.getSelection();
        if (selection) {
          navigator.clipboard.writeText(selection);
          return false; // prevent default
        }
      }
      return true;
    });

    xtermRef.current = terminal;
    fitAddonRef.current = fitAddon;
    searchAddonRef.current = searchAddon;
    lastLogIndexRef.current = 0;

    // Handle user scroll (disable auto-scroll when scrolled up)
    terminal.onScroll(() => {
      const buffer = terminal.buffer.active;
      const atBottom = buffer.baseY + terminal.rows >= buffer.length;
      autoScrollRef.current = atBottom;
    });

    // Resize observer
    const observer = new ResizeObserver(() => {
      try { fitAddon.fit(); } catch {}
    });
    observer.observe(termRef.current);

    return () => {
      observer.disconnect();
      terminal.dispose();
      xtermRef.current = null;
      fitAddonRef.current = null;
      searchAddonRef.current = null;
      lastLogIndexRef.current = 0;
    };
  }, [commandId]);

  // Write new logs to terminal
  useEffect(() => {
    const terminal = xtermRef.current;
    if (!terminal) return;

    const startIdx = lastLogIndexRef.current;
    if (startIdx < logs.length) {
      for (let i = startIdx; i < logs.length; i++) {
        terminal.writeln(logs[i]);
      }
      lastLogIndexRef.current = logs.length;

      if (autoScrollRef.current) {
        terminal.scrollToBottom();
      }
    }

    // Handle log clear (when logs array is shorter than what we've written)
    if (logs.length < lastLogIndexRef.current) {
      terminal.clear();
      lastLogIndexRef.current = 0;
      for (let i = 0; i < logs.length; i++) {
        terminal.writeln(logs[i]);
      }
      lastLogIndexRef.current = logs.length;
    }
  }, [logs]);

  // Expose search addon for toolbar
  useEffect(() => {
    const el = termRef.current;
    if (el && searchAddonRef.current) {
      (el as any).__searchAddon = searchAddonRef.current;
      (el as any).__terminal = xtermRef.current;
    }
  }, [commandId]);

  return (
    <div className="h-full flex flex-col">
      <div className="flex items-center justify-between px-3 py-1.5 bg-zinc-800/50 border-b border-zinc-700/50">
        <ResourceMonitor commandId={commandId} />
        <ServerControls commandId={commandId} />
      </div>
      <div ref={termRef} className="flex-1 bg-[#09090b] px-1" />
    </div>
  );
}
